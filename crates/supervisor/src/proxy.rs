//! Serves the runtime's session endpoints (MCP, memory) to the agent on localhost, attaching the pod's ServiceAccount
//! token to every request, so the runtime knows which session is calling from the pod's own identity (#196) rather
//! than from its IP.

use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use sloppod_protocol::link::LOCAL_PROXY;

#[derive(Clone)]
struct Proxy {
    runtime: String,
    token_file: PathBuf,
    http: reqwest::Client,
}

/// Binds the local address; the agent is only pointed here once this has worked.
pub async fn bind() -> anyhow::Result<tokio::net::TcpListener> {
    Ok(tokio::net::TcpListener::bind(LOCAL_PROXY).await?)
}

/// Serves until the supervisor exits. Streams both ways: MCP uses server-sent events.
pub async fn serve(
    listener: tokio::net::TcpListener,
    runtime: String,
    token_file: PathBuf,
) -> anyhow::Result<()> {
    let proxy = Proxy {
        runtime: runtime.trim_end_matches('/').to_owned(),
        token_file,
        http: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?,
    };
    let app = Router::new()
        .route("/mcp", any(forward))
        .route("/memini", any(forward))
        .route("/memini/{*rest}", any(forward))
        .route("/servers/{*rest}", any(forward))
        .with_state(proxy);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn forward(State(proxy): State<Proxy>, request: Request) -> Response {
    match send(&proxy, request).await {
        Ok(response) => response,
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("sloppod: the runtime isn't reachable: {e:#}\n"),
        )
            .into_response(),
    }
}

fn hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

async fn send(proxy: &Proxy, request: Request) -> anyhow::Result<Response> {
    // Read each time: the kubelet rotates it on disk.
    let token = tokio::fs::read_to_string(&proxy.token_file).await?;
    let (parts, body) = request.into_parts();
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let mut upstream = proxy
        .http
        .request(parts.method, format!("{}{path}", proxy.runtime));
    for (name, value) in &parts.headers {
        // The agent's own credential (memini's client sends a placeholder) never goes on; the pod's does.
        if hop_by_hop(name)
            || name == header::HOST
            || name == header::CONTENT_LENGTH
            || name == header::AUTHORIZATION
        {
            continue;
        }
        upstream = upstream.header(name, value);
    }
    let response = upstream
        .bearer_auth(token.trim())
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await?;
    let mut proxied = Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if !hop_by_hop(name) && name != header::CONTENT_LENGTH {
            proxied = proxied.header(name, value);
        }
    }
    Ok(proxied.body(Body::from_stream(response.bytes_stream()))?)
}
