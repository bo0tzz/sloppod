use std::sync::Arc;

use async_trait::async_trait;
use sloppod_contracts::capability;
use sloppod_contracts::{
    Decision, Error, HttpRequest, Manifest, RequestPolicy, Result, SessionContext,
};

use crate::credentials::Credentials;

/// Name of the session secret holding the session's own memini key (ADR 22).
pub const SESSION_KEY: &str = "memini-key";

/// memini: the session's own key, and the namespace the runtime chose for it (ADR 8).
pub struct MeminiPolicy {
    credentials: Arc<Credentials>,
}

impl MeminiPolicy {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }
}

fn administrative(method: &str, segments: &[String]) -> bool {
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    match (method, segments.as_slice()) {
        (_, ["v1", "keys", ..]) => true,
        (_, ["v1", "fsck" | "dedup"]) => true,
        (_, ["v1", "namespaces", "move" | "split"]) => true,
        ("DELETE", ["v1", "namespaces"]) => true,
        (m, ["v1", "pins" | "links"]) if m != "GET" => true,
        ("PUT", ["v1", "self", "settings"] | ["v1", "settings", "defaults"]) => true,
        ("DELETE", ["v1", "memories"]) => true,
        (_, ["v1", "memories", _, "reassign"]) => true,
        (_, [".well-known", ..] | ["register"] | ["authorize"] | ["token"]) => true,
        _ => false,
    }
}

#[async_trait]
impl RequestPolicy for MeminiPolicy {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("memory-memini")
    }

    /// None: sessions reach memini through the runtime's `/memini` route, not the egress listener.
    fn hosts(&self) -> Vec<String> {
        Vec::new()
    }

    async fn decide(&self, session: &SessionContext, request: &HttpRequest) -> Result<Decision> {
        let path = request.path_only();
        let Ok(segments) = request.segments() else {
            return Ok(Decision::deny(format!(
                "sloppod: {path} isn't a path memini takes.\n"
            )));
        };
        if administrative(&request.method, &segments) {
            return Ok(Decision::deny(format!(
                "sloppod: memini administration ({} {path}) isn't available to sessions; the runtime manages \
                 namespaces, pins, links and keys.\n",
                request.method
            )));
        }
        let Some(namespace) = session
            .grant(capability::MEMORY)
            .and_then(|g| g.param("namespace"))
        else {
            return Ok(Decision::deny(
                "sloppod: this session has no memory access.\n",
            ));
        };
        let session_id = session
            .session_id
            .parse()
            .map_err(|_| Error::new("bad session id"))?;
        let key = self
            .credentials
            .session_secret(session_id, SESSION_KEY)
            .await
            .map_err(|e| Error::new(e.to_string()))?;
        let Some(key) = key else {
            return Ok(Decision::deny("sloppod: this session has no memini key.\n"));
        };
        Ok(Decision::allow_with(
            vec![
                ("authorization".into(), format!("Bearer {key}")),
                ("x-memini-namespace".into(), namespace.to_owned()),
            ],
            vec!["x-memini-home".into()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(path: &str) -> Vec<String> {
        sloppod_contracts::path_segments(path).unwrap()
    }

    #[test]
    fn administration_is_refused() {
        for (method, path) in [
            ("POST", "/v1/keys"),
            ("GET", "/v1/keys"),
            ("PUT", "/v1/pins"),
            ("POST", "/v1/links"),
            ("POST", "/v1/namespaces/move"),
            ("PUT", "/v1/self/settings"),
            ("DELETE", "/v1/memories"),
            ("POST", "/register"),
            ("GET", "/.well-known/oauth-authorization-server"),
        ] {
            assert!(administrative(method, &segments(path)), "{method} {path}");
        }
        for (method, path) in [
            ("POST", "/mcp"),
            ("POST", "/v1/handshake"),
            ("POST", "/v1/memories"),
            ("DELETE", "/v1/memories/abc"),
            ("POST", "/v1/search"),
            ("GET", "/v1/namespaces/briefing"),
            ("GET", "/v1/pins"),
        ] {
            assert!(!administrative(method, &segments(path)), "{method} {path}");
        }
    }
}
