use async_trait::async_trait;
use sloppod_contracts::{Decision, HttpRequest, Manifest, RequestPolicy, Result, SessionContext};

/// memini: the runtime's key, and the namespace the runtime chose for the session (ADR 8).
pub struct MeminiPolicy {
    host: String,
    api_key: String,
}

impl MeminiPolicy {
    pub fn new(host: String, api_key: String) -> Self {
        Self { host, api_key }
    }
}

fn administrative(method: &str, path: &str) -> bool {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
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

    fn hosts(&self) -> Vec<String> {
        vec![self.host.clone()]
    }

    async fn decide(&self, session: &SessionContext, request: &HttpRequest) -> Result<Decision> {
        let path = request.path_only();
        if administrative(&request.method, path) {
            return Ok(Decision::deny(format!(
                "sloppod: memini administration ({} {path}) isn't available to sessions; the runtime manages \
                 namespaces, pins, links and keys.\n",
                request.method
            )));
        }
        let Some(namespace) = &session.memory_namespace else {
            return Ok(Decision::deny(
                "sloppod: this session has no memory namespace.\n",
            ));
        };
        Ok(Decision::allow_with(
            vec![
                ("authorization".into(), format!("Bearer {}", self.api_key)),
                ("x-memini-namespace".into(), namespace.clone()),
            ],
            vec!["x-memini-home".into()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(administrative(method, path), "{method} {path}");
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
            assert!(!administrative(method, path), "{method} {path}");
        }
    }
}
