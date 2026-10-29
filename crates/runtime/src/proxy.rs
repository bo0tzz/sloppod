//! Forwarding a session's HTTP request to a service the runtime fronts for it (memini, a capability's MCP server),
//! with the headers the runtime attaches, and streaming the answer back.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, Method, header};
use axum::response::Response;

/// The client for services the deployment configures (memini). A redirect goes back to the session's client, as any
/// other answer does, rather than being followed from the runtime.
pub static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("building the proxy client")
});

/// Where requests to servers an owner names (their definitions' URLs) can't go: the runtime itself, by its pod and
/// Service addresses or loopback, and the node (link-local, which covers cloud metadata). A request from the runtime
/// carries the runtime's own standing, so a URL pointing back at it would let a session use the user API.
/// In-cluster servers elsewhere stay reachable.
pub struct Reach {
    own: Arc<[IpAddr]>,
    http: reqwest::Client,
}

/// Whether a connection to `ip` may be made for a session.
fn reachable(own: &[IpAddr], ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    let local = match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_link_local() || v4.octets()[0] == 0,
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || v6.is_unicast_link_local(),
    };
    !local && !own.iter().any(|o| o.to_canonical() == ip)
}

/// Resolves names as the system does, keeping only the addresses [`reachable`] allows, so a name can't be moved onto
/// the runtime after it was checked: this runs for every connection.
struct Resolver(Arc<[IpAddr]>);

impl reqwest::dns::Resolve for Resolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let own = self.0.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let allowed: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| reachable(&own, addr.ip()))
                .collect();
            if allowed.is_empty() {
                return Err(format!("{host} is the runtime itself or a local address").into());
            }
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

impl Reach {
    /// `own` is the runtime's addresses: its pod's and its Service's.
    pub fn new(own: Vec<IpAddr>) -> Self {
        let own: Arc<[IpAddr]> = own.into();
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            // Through a proxy, the address checked wouldn't be the one the request goes to.
            .no_proxy()
            .dns_resolver(Arc::new(Resolver(own.clone())))
            .build()
            .expect("building the servers' proxy client");
        Self { own, http }
    }

    /// Why a session's request can't be sent to `url`, if it can't. Names are resolved now, as well as again on
    /// every connection; one that doesn't resolve yet isn't refused here.
    pub async fn refuses(&self, url: &str) -> Option<String> {
        let refusal =
            || Some("the server can't be the runtime itself or a local address".to_owned());
        match host_ip(url)? {
            Ok(ip) if !reachable(&self.own, ip) => refusal(),
            Ok(_) => None,
            Err(host) => {
                let mut addrs = tokio::net::lookup_host((host.as_str(), 0)).await.ok()?;
                addrs
                    .any(|addr| !reachable(&self.own, addr.ip()))
                    .then(refusal)
                    .flatten()
            }
        }
    }

    /// Sends a session's request on to `url`, as [`forward`] does, unless `url` is somewhere it can't go.
    pub async fn forward(&self, request: Forward<'_>) -> anyhow::Result<Response> {
        // An address in the URL is connected to without the resolver.
        if let Some(Ok(ip)) = host_ip(request.url)
            && !reachable(&self.own, ip)
        {
            anyhow::bail!("the server can't be the runtime itself or a local address");
        }
        forward(&self.http, request).await
    }
}

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

/// A URL's host: the address it names, or the name to resolve. `None` for one that doesn't parse.
fn host_ip(url: &str) -> Option<Result<IpAddr, String>> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    Some(bare.parse().map_err(|_| host.to_owned()))
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

/// A session's request to send on.
pub struct Forward<'a> {
    pub method: Method,
    pub headers: &'a HeaderMap,
    pub url: &'a str,
    pub body: Bytes,
    /// Headers the runtime attaches, replacing any of the request's by those names.
    pub set: Vec<(String, String)>,
    /// The request's headers to drop.
    pub remove: Vec<String>,
    /// The deployment's user header (ADR 23), dropped with the other forwarding headers.
    pub user_header: &'a str,
}

/// Sends the request on with `client`: its headers, less hop-by-hop and forwarding ones and those `set` replaces or
/// `remove` drops, plus `set`; the answer streams back as it comes.
pub async fn forward(client: &reqwest::Client, request: Forward<'_>) -> anyhow::Result<Response> {
    let Forward {
        method,
        headers,
        url,
        body,
        set,
        remove,
        user_header,
    } = request;
    let mut upstream = client.request(method, url);
    for (name, value) in headers {
        let replaced = set
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(name.as_str()));
        let removed = remove.iter().any(|n| n.eq_ignore_ascii_case(name.as_str()));
        if hop_by_hop(name)
            || sloppod_contracts::forwarding_header(name.as_str(), user_header)
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

    #[test]
    fn the_runtime_and_local_addresses_are_out_of_reach() {
        let own: Vec<IpAddr> = vec!["10.96.0.10".parse().unwrap(), "10.244.1.5".parse().unwrap()];
        for ip in [
            "127.0.0.1",
            "127.5.5.5",
            "::1",
            "0.0.0.0",
            "::",
            "169.254.169.254",
            "fe80::1",
            "::ffff:127.0.0.1",
            "10.96.0.10",
            "10.244.1.5",
            "::ffff:10.244.1.5",
        ] {
            assert!(!reachable(&own, ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "10.96.0.11",
            "10.244.2.7",
            "192.168.1.20",
            "8.8.8.8",
            "fd00::5",
        ] {
            assert!(reachable(&own, ip.parse().unwrap()), "{ip}");
        }
    }

    /// A server on loopback, standing in for the runtime's API, that answers with the headers it got and counts
    /// requests; `/moved` redirects to `/`.
    async fn echo() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        use axum::response::IntoResponse;
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = hits.clone();
        let app = axum::Router::new()
            .route(
                "/moved",
                axum::routing::get(|| async { axum::response::Redirect::to("/") }),
            )
            .fallback(move |headers: HeaderMap| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    let names: Vec<String> = headers.keys().map(|n| n.to_string()).collect();
                    axum::Json(names).into_response()
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (port, hits)
    }

    fn request<'a>(headers: &'a HeaderMap, url: &'a str) -> Forward<'a> {
        Forward {
            method: Method::GET,
            headers,
            url,
            body: Bytes::new(),
            set: Vec::new(),
            remove: Vec::new(),
            user_header: "x-sloppod-user",
        }
    }

    #[tokio::test]
    async fn a_server_can_t_be_the_runtime_s_api() {
        let (port, hits) = echo().await;
        let reach = Reach::new(vec!["10.96.0.10".parse().unwrap()]);
        let headers = HeaderMap::new();
        for url in [
            format!("http://localhost:{port}/api/v1/proposals"),
            format!("http://127.0.0.1:{port}/api/v1/proposals"),
            format!("http://[::ffff:127.0.0.1]:{port}/api/v1/proposals"),
            format!("http://2130706433:{port}/api/v1/proposals"),
            "http://10.96.0.10:8080/api/v1/proposals".to_owned(),
        ] {
            assert!(reach.refuses(&url).await.is_some(), "{url}");
            assert!(
                reach.forward(request(&headers, &url)).await.is_err(),
                "{url}"
            );
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(reach.refuses("http://10.96.0.11:8080/mcp").await, None);
    }

    #[tokio::test]
    async fn who_a_request_is_from_is_never_passed_on() {
        let (port, _) = echo().await;
        let mut headers = HeaderMap::new();
        for name in [
            "x-forwarded-user",
            "x-sloppod-user",
            "x-forwarded-for",
            "x-forwarded-email",
            "forwarded",
            "x-real-ip",
            "x-kept",
        ] {
            headers.insert(HeaderName::from_static(name), "admin".parse().unwrap());
        }
        let url = format!("http://127.0.0.1:{port}/");
        let response = forward(&HTTP, request(&headers, &url)).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let got: Vec<String> = serde_json::from_slice(&body).unwrap();
        assert!(got.contains(&"x-kept".to_owned()), "{got:?}");
        for name in &got {
            assert!(
                !sloppod_contracts::forwarding_header(name, "x-sloppod-user"),
                "{name} reached the server"
            );
        }
    }

    #[tokio::test]
    async fn redirects_go_back_to_the_session() {
        let (port, hits) = echo().await;
        let headers = HeaderMap::new();
        let url = format!("http://127.0.0.1:{port}/moved");
        let response = forward(&HTTP, request(&headers, &url)).await.unwrap();
        assert!(response.status().is_redirection());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
