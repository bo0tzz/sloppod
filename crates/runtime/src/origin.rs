//! Only sloppod's own pages act as the user from a browser (#66). The runtime trusts the authenticating proxy in
//! front of it for who a request comes from, and that proxy may know the user from a cookie the browser sends along
//! even when another site's page makes the request. A browser always says which origin a write or a WebSocket comes
//! from (`Origin`), and a page can't change it, so those are refused unless it's sloppod's own. A request without
//! `Origin` isn't from a page, e.g. `sloppodctl`.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;

pub async fn same_origin(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if checked(request.method(), request.headers())
        && !origin_allowed(request.headers(), &state.config.allowed_origins)
    {
        return (
            StatusCode::FORBIDDEN,
            "sloppod: only sloppod's own pages can do this from a browser",
        )
            .into_response();
    }
    next.run(request).await
}

/// Writes, and WebSockets, which are opened with a GET.
fn checked(method: &Method, headers: &HeaderMap) -> bool {
    let reads = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    let websocket = headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    !reads || websocket
}

/// Whether a request's `Origin` is sloppod's own: the scheme and host the browser asked for, as the proxy passed them
/// on (`X-Forwarded-Proto`, `X-Forwarded-Host`, or the request's own `Host` over plain HTTP), or one of `allowed`.
fn origin_allowed(headers: &HeaderMap, allowed: &[String]) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if allowed
        .iter()
        .any(|a| a.trim_end_matches('/').eq_ignore_ascii_case(origin))
    {
        return true;
    }
    let first = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
    };
    let scheme = first("x-forwarded-proto").unwrap_or("http");
    [first("x-forwarded-host"), first("host")]
        .into_iter()
        .flatten()
        .any(|host| origin.eq_ignore_ascii_case(&format!("{scheme}://{host}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, value.parse().unwrap());
        }
        headers
    }

    const NONE: &[String] = &[];

    #[test]
    fn sloppods_own_pages_get_through() {
        let behind_proxy = |origin: &str| {
            headers(&[
                ("origin", origin),
                ("host", "sloppod.slop.svc:8080"),
                ("x-forwarded-host", "sloppod.example.com"),
                ("x-forwarded-proto", "https"),
            ])
        };
        assert!(origin_allowed(
            &behind_proxy("https://sloppod.example.com"),
            NONE
        ));
        assert!(origin_allowed(
            &headers(&[
                ("origin", "http://localhost:18080"),
                ("host", "localhost:18080")
            ]),
            NONE
        ));
        // Not from a page at all.
        assert!(origin_allowed(
            &headers(&[("host", "sloppod.example.com")]),
            NONE
        ));

        // Other sites, look-alikes, the same host over plain HTTP, and opaque origins don't.
        for origin in [
            "https://evil.example",
            "https://sloppod.example.com.evil.example",
            "http://sloppod.example.com",
            "https://sloppod.example.com:8443",
            "null",
        ] {
            assert!(!origin_allowed(&behind_proxy(origin), NONE), "{origin}");
        }
    }

    #[test]
    fn other_origins_can_be_allowed() {
        let request = headers(&[
            ("origin", "https://ui.example.com"),
            ("host", "sloppod.example.com"),
        ]);
        assert!(!origin_allowed(&request, NONE));
        assert!(origin_allowed(
            &request,
            &["https://ui.example.com/".to_owned()]
        ));
    }

    #[test]
    fn writes_and_websockets_are_checked() {
        let none = HeaderMap::new();
        assert!(checked(&Method::POST, &none));
        assert!(checked(&Method::DELETE, &none));
        assert!(!checked(&Method::GET, &none));
        assert!(checked(&Method::GET, &headers(&[("upgrade", "websocket")])));
    }
}
