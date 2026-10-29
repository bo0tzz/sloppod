//! Forwarding a session's HTTP request to a service the runtime fronts for it (memini, a capability's MCP server),
//! with the headers the runtime attaches, and streaming the answer back.

use std::sync::LazyLock;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, Method, header};
use axum::response::Response;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("building the proxy client")
});

/// Headers that belong to one connection, not the request, and aren't forwarded.
fn hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authorization"
    )
}

/// A request path made canonical: its segments decoded where that's safe and re-encoded, so what's checked is what's
/// sent. Dot segments are refused rather than resolved.
pub fn canonical_path(path_and_query: &str) -> anyhow::Result<String> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query, None),
    };
    let segments = sloppod_contracts::path_segments(path)?;
    let mut canonical = reqwest::Url::parse("http://proxy.invalid/")?;
    canonical
        .path_segments_mut()
        .map_err(|()| anyhow::anyhow!("no path"))?
        .extend(&segments);
    canonical.set_query(query);
    Ok(match canonical.query() {
        Some(query) => format!("{}?{query}", canonical.path()),
        None => canonical.path().to_owned(),
    })
}

/// Sends the request on to `url`: its headers, less hop-by-hop ones and those `set` replaces or `remove` drops,
/// plus `set`; the answer streams back as it comes.
pub async fn forward(
    method: Method,
    headers: &HeaderMap,
    url: &str,
    body: Bytes,
    set: Vec<(String, String)>,
    remove: Vec<String>,
) -> anyhow::Result<Response> {
    let mut upstream = HTTP.request(method, url);
    for (name, value) in headers {
        let replaced = set
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(name.as_str()));
        let removed = remove.iter().any(|n| n.eq_ignore_ascii_case(name.as_str()));
        if hop_by_hop(name)
            || name == header::HOST
            || name == header::CONTENT_LENGTH
            || replaced
            || removed
        {
            continue;
        }
        upstream = upstream.header(name, value);
    }
    for (name, value) in set {
        upstream = upstream.header(name, value);
    }
    let response = upstream.body(body).send().await?;
    let mut proxied = Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if !hop_by_hop(name) && name != header::CONTENT_LENGTH {
            proxied = proxied.header(name, value);
        }
    }
    Ok(proxied.body(Body::from_stream(response.bytes_stream()))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_canonical() {
        for (raw, canonical) in [
            ("/v1/memories", "/v1/memories"),
            ("/v1/%6Beys", "/v1/keys"),
            ("/v1//keys/", "/v1/keys"),
            ("/v1/memories?q=a%20b", "/v1/memories?q=a%20b"),
            ("/v1/namespaces/a%2Fb", "/v1/namespaces/a%2Fb"),
            ("/", "/"),
        ] {
            assert_eq!(canonical_path(raw).unwrap(), canonical, "{raw}");
        }
        for raw in [
            "/v1/x/../pins",
            "/v1/x/%2e%2e/keys",
            "/v1/./memories",
            "/../../v1/keys",
        ] {
            assert!(canonical_path(raw).is_err(), "{raw}");
        }
    }
}
