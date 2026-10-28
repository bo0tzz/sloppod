//! Which session a request on the sessions port comes from: its pod's ServiceAccount token, which the supervisor
//! attaches (#196), verified by the Kubernetes API like the supervisor link's. Pods whose supervisor predates that
//! send no token, and are still found by their IP until they're replaced.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, header};
use uuid::Uuid;

use crate::AppState;

/// How long a verified token is trusted without asking the API again. Tokens live an hour or more, and every agent
/// request would otherwise cost a TokenReview.
const TRUST_FOR: Duration = Duration::from_secs(60);

static VERIFIED: LazyLock<Mutex<HashMap<String, (Uuid, Instant)>>> =
    LazyLock::new(Default::default);

/// The calling session, or `None` if the caller isn't one.
pub async fn calling_session(state: &AppState, headers: &HeaderMap, peer: IpAddr) -> Option<Uuid> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let Some(token) = token else {
        let session = state.pods.session_for_ip(&peer.to_string());
        if let Some(session) = session {
            tracing::warn!(%session, "a session called without its pod's token; found by IP");
        }
        return session;
    };
    if let Some(session) = cached(token) {
        return Some(session);
    }
    match crate::link::authenticate(state, token).await {
        Ok(session) => {
            let mut verified = VERIFIED.lock().unwrap();
            verified.retain(|_, (_, at)| at.elapsed() < TRUST_FOR);
            verified.insert(token.to_owned(), (session, Instant::now()));
            Some(session)
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "a session token that doesn't check out");
            None
        }
    }
}

fn cached(token: &str) -> Option<Uuid> {
    let verified = VERIFIED.lock().unwrap();
    verified
        .get(token)
        .filter(|(_, at)| at.elapsed() < TRUST_FOR)
        .map(|(session, _)| *session)
}
