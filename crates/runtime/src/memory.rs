//! Sessions reach memini through the runtime, on the sessions port, so memini can live at any address the runtime
//! can reach, including one inside the cluster. The memini policy decides each request (ADR 8).

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Context;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sloppod_contracts::{Decision, HttpRequest, RequestPolicy};

use crate::credentials::MEMINI;
use crate::{AppState, db};

/// Where sessions find memini on the runtime's sessions port.
pub const PREFIX: &str = "/memini";

/// The memini path and query a request is for, in the one form the policy checks and memini receives: read by the
/// shared path parser, then encoded again, so a `/` within a segment (as in a namespace) stays within it.
fn canonical_path(path_and_query: &str) -> anyhow::Result<String> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query, None),
    };
    let segments = sloppod_contracts::path_segments(path)?;
    let mut canonical = reqwest::Url::parse("http://memini.invalid/")?;
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
const MAX_BODY: usize = 8 << 20;

/// Streams responses (MCP uses server-sent events), so there's no overall timeout.
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("building the memini client")
});

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

pub async fn proxy(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    match forward(&state, peer, request).await {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!(%peer, error = %format!("{e:#}"), "proxying to memini");
            (
                StatusCode::BAD_GATEWAY,
                format!("sloppod: memini isn't reachable: {e:#}\n"),
            )
                .into_response()
        }
    }
}

async fn forward(state: &AppState, peer: SocketAddr, request: Request) -> anyhow::Result<Response> {
    let Some(session) = crate::identity::calling_session(state, request.headers(), peer.ip()).await
    else {
        return Ok((
            StatusCode::FORBIDDEN,
            "sloppod: memory is only for session pods\n",
        )
            .into_response());
    };
    let session = db::get_session(&state.db, session)
        .await?
        .context("unknown session")?;
    // Only its URL is needed here; the admin key stays sealed.
    let Some(credential) = state
        .credentials
        .default_row(&session.owner, MEMINI)
        .await?
    else {
        return Ok((StatusCode::NOT_FOUND, "sloppod: no memini is configured\n").into_response());
    };
    let base = credential
        .config("url")
        .context("the memini credential has no URL")?
        .trim_end_matches('/')
        .to_owned();

    let (parts, body) = request.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let path = match path_and_query.strip_prefix(PREFIX) {
        Some(rest) if rest.starts_with('/') => rest.to_owned(),
        Some(rest) => format!("/{rest}"),
        None => path_and_query.to_owned(),
    };
    let Ok(path) = canonical_path(&path) else {
        return Ok((StatusCode::BAD_REQUEST, "sloppod: invalid memini path\n").into_response());
    };
    let body = axum::body::to_bytes(body, MAX_BODY)
        .await
        .context("reading the request body")?;
    let headers: BTreeMap<String, String> = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect();
    let decision = state
        .memory
        .decide(
            &session.context(),
            &HttpRequest {
                method: parts.method.to_string(),
                host: reqwest::Url::parse(&base)?
                    .host_str()
                    .unwrap_or_default()
                    .to_owned(),
                path: path.clone(),
                headers,
                body: body.to_vec(),
                body_truncated: false,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.0))?;
    let (set_headers, remove_headers) = match decision {
        Decision::Allow {
            set_headers,
            remove_headers,
        } => sloppod_contracts::header_ops(set_headers, remove_headers),
        Decision::Deny {
            status,
            message,
            content_type,
        } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
            let content_type = content_type.unwrap_or_else(|| "text/plain; charset=utf-8".into());
            return Ok((status, [(header::CONTENT_TYPE, content_type)], message).into_response());
        }
    };

    let mut upstream = HTTP.request(parts.method, format!("{base}{path}"));
    for (name, value) in &parts.headers {
        let replaced = set_headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(name.as_str()));
        let removed = remove_headers
            .iter()
            .any(|n| n.eq_ignore_ascii_case(name.as_str()));
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
    for (name, value) in set_headers {
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
        // Dot segments are refused, not resolved: the policy and memini can't read them differently.
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
