//! Who a request comes from (ADR 23): the authenticating proxy's user header, or the configured default user.

use axum::Json;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde_json::json;

use crate::AppState;

#[derive(Debug, Clone)]
pub struct User(pub String);

impl FromRequestParts<AppState> for User {
    type Rejection = (StatusCode, Json<serde_json::Value>);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Session pods only get the sessions port, but never let them act as a user even if they got through.
        if let Some(ConnectInfo(peer)) = parts.extensions.get::<ConnectInfo<SocketAddr>>()
            && state.pods.session_for_ip(&peer.ip().to_string()).is_some()
        {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({"error": "session pods can't use the API"})),
            ));
        }
        let from_header = parts
            .headers
            .get(state.config.user_header.as_str())
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty());
        match from_header.or(state.config.default_user.as_deref()) {
            Some(user) => Ok(User(user.to_owned())),
            None => Err((
                StatusCode::UNAUTHORIZED,
                Json(
                    json!({"error": format!("no user: the request has no {} header", state.config.user_header)}),
                ),
            )),
        }
    }
}
