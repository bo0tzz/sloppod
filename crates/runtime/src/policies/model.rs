use std::sync::Arc;

use async_trait::async_trait;
use sloppod_contracts::capability;
use sloppod_contracts::{
    Decision, Error, HttpRequest, Manifest, RequestPolicy, Result, SessionContext,
};

use crate::credentials::{CLAUDE, Credentials};

const INFERENCE: &[&str] = &["/v1/messages", "/v1/messages/count_tokens"];

/// The Anthropic API: inference only, with the owner's Claude account (ADR 11 spike results).
pub struct ModelPolicy {
    credentials: Arc<Credentials>,
}

impl ModelPolicy {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }
}

fn is_inference(request: &HttpRequest) -> bool {
    request.method == "POST" && INFERENCE.contains(&request.path_only())
}

#[async_trait]
impl RequestPolicy for ModelPolicy {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("model-anthropic")
    }

    fn hosts(&self) -> Vec<String> {
        vec!["api.anthropic.com".into()]
    }

    async fn decide(&self, session: &SessionContext, request: &HttpRequest) -> Result<Decision> {
        if !session.has(capability::MODEL) {
            return Ok(Decision::deny(
                "sloppod: this session has no model access.\n",
            ));
        }
        if !is_inference(request) {
            return Ok(Decision::deny(format!(
                "sloppod: sessions may only call the model API's inference endpoint (POST /v1/messages); {} {} was refused.\n",
                request.method,
                request.path_only()
            )));
        }
        let account = self
            .credentials
            .secret(&session.owner, CLAUDE)
            .await
            .map_err(|e| Error::new(e.to_string()))?;
        let Some((_, token)) = account else {
            return Ok(Decision::deny(format!(
                "sloppod: {} has no Claude account configured; add one in sloppod's settings.\n",
                session.owner
            )));
        };
        Ok(Decision::allow_with(
            vec![("authorization".into(), format!("Bearer {token}"))],
            vec!["x-api-key".into()],
        ))
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

    #[test]
    fn only_inference_is_allowed() {
        assert!(is_inference(&request("POST", "/v1/messages?beta=true")));
        assert!(is_inference(&request("POST", "/v1/messages/count_tokens")));
        for (method, path) in [
            ("GET", "/api/claude_code/settings"),
            ("POST", "/api/event_logging/v2/batch"),
            ("GET", "/v1/messages"),
        ] {
            assert!(!is_inference(&request(method, path)), "{method} {path}");
        }
    }
}
