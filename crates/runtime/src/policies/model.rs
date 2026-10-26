use async_trait::async_trait;
use sloppod_contracts::{Decision, HttpRequest, Manifest, RequestPolicy, Result, SessionContext};

const INFERENCE: &[&str] = &["/v1/messages", "/v1/messages/count_tokens"];

/// The Anthropic API: inference only, with the runtime's OAuth token (ADR 11 spike results).
pub struct ModelPolicy {
    oauth_token: String,
}

impl ModelPolicy {
    pub fn new(oauth_token: String) -> Self {
        Self { oauth_token }
    }
}

#[async_trait]
impl RequestPolicy for ModelPolicy {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("model-anthropic")
    }

    fn hosts(&self) -> Vec<String> {
        vec!["api.anthropic.com".into()]
    }

    async fn decide(&self, _session: &SessionContext, request: &HttpRequest) -> Result<Decision> {
        if request.method == "POST" && INFERENCE.contains(&request.path_only()) {
            return Ok(Decision::allow_with(
                vec![(
                    "authorization".into(),
                    format!("Bearer {}", self.oauth_token),
                )],
                vec!["x-api-key".into()],
            ));
        }
        Ok(Decision::deny(format!(
            "sloppod: sessions may only call the model API's inference endpoint (POST /v1/messages); {} {} was refused.\n",
            request.method,
            request.path_only()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn request(method: &str, path: &str) -> HttpRequest {
        HttpRequest {
            method: method.into(),
            host: "api.anthropic.com".into(),
            path: path.into(),
            headers: BTreeMap::new(),
            body: Vec::new(),
            body_truncated: false,
        }
    }

    fn session() -> SessionContext {
        SessionContext {
            session_id: "s".into(),
            repo: "o/r".into(),
            branch: "sloppod/s".into(),
            memory_namespace: None,
        }
    }

    #[tokio::test]
    async fn inference_gets_the_token() {
        let policy = ModelPolicy::new("real".into());
        let decision = policy
            .decide(&session(), &request("POST", "/v1/messages?beta=true"))
            .await
            .unwrap();
        assert_eq!(
            decision,
            Decision::allow_with(
                vec![("authorization".into(), "Bearer real".into())],
                vec!["x-api-key".into()]
            )
        );
    }

    #[tokio::test]
    async fn everything_else_is_refused() {
        let policy = ModelPolicy::new("real".into());
        for (method, path) in [
            ("GET", "/api/claude_code/settings"),
            ("POST", "/api/event_logging/v2/batch"),
            ("GET", "/v1/messages"),
        ] {
            let decision = policy
                .decide(&session(), &request(method, path))
                .await
                .unwrap();
            assert!(
                matches!(decision, Decision::Deny { status: 403, .. }),
                "{method} {path}"
            );
        }
    }
}
