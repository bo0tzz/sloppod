use std::sync::Arc;

use async_trait::async_trait;
use sloppod_contracts::capability;
use sloppod_contracts::{
    Decision, Error, HttpRequest, Manifest, RequestPolicy, Result, SessionContext,
};

use crate::credentials::Credentials;

/// Codex's model API on the ChatGPT backend, reached with the owner's ChatGPT subscription (ADR 24).
const CODEX_PATH: &str = "/backend-api/codex/";

/// Codex through a ChatGPT login: only Codex's own backend API, with the owner's token and account attached.
pub struct CodexPolicy {
    credentials: Arc<Credentials>,
}

impl CodexPolicy {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }
}

/// Envoy passes the path upstream as sent, so a `..` (or `%2e%2e`, or an empty segment) after the prefix could reach
/// another ChatGPT API with the owner's token. Every segment after it has to be a plain name.
fn is_codex(request: &HttpRequest) -> bool {
    let Some(rest) = request.path_only().strip_prefix(CODEX_PATH) else {
        return false;
    };
    matches!(request.method.as_str(), "GET" | "POST")
        && rest.split('/').all(|segment| {
            let decoded = percent_encoding::percent_decode_str(segment).decode_utf8_lossy();
            !matches!(decoded.as_ref(), "" | "." | "..") && !decoded.contains(['/', '\\'])
        })
}

#[async_trait]
impl RequestPolicy for CodexPolicy {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("model-codex")
    }

    fn hosts(&self) -> Vec<String> {
        vec!["chatgpt.com".into()]
    }

    async fn decide(&self, session: &SessionContext, request: &HttpRequest) -> Result<Decision> {
        if !session.has(capability::CODEX) {
            return Ok(Decision::deny(
                "sloppod: this session has no Codex access.\n",
            ));
        }
        if !is_codex(request) {
            return Ok(Decision::deny(format!(
                "sloppod: sessions may only call Codex's API ({CODEX_PATH}) on chatgpt.com; {} {} was refused.\n",
                request.method,
                request.path_only()
            )));
        }
        let tokens = self
            .credentials
            .chatgpt(&session.owner)
            .await
            .map_err(|e| Error::new(e.to_string()))?;
        let Some(tokens) = tokens else {
            return Ok(Decision::deny(format!(
                "sloppod: {} has no ChatGPT login configured; add one in sloppod's settings.\n",
                session.owner
            )));
        };
        Ok(Decision::allow_with(
            vec![
                (
                    "authorization".into(),
                    format!("Bearer {}", tokens.access_token),
                ),
                ("chatgpt-account-id".into(), tokens.account_id),
            ],
            vec!["cookie".into()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, path: &str) -> HttpRequest {
        HttpRequest {
            method: method.into(),
            host: "chatgpt.com".into(),
            path: path.into(),
            headers: Default::default(),
            body: Vec::new(),
            body_truncated: false,
        }
    }

    #[test]
    fn only_codex_is_allowed() {
        assert!(is_codex(&request("POST", "/backend-api/codex/responses")));
        assert!(is_codex(&request(
            "GET",
            "/backend-api/codex/models?client_version=1"
        )));
        assert!(!is_codex(&request("POST", "/backend-api/conversation")));
        assert!(!is_codex(&request("GET", "/backend-api/codexfoo")));
        assert!(!is_codex(&request(
            "DELETE",
            "/backend-api/codex/responses"
        )));
        for escape in [
            "/backend-api/codex/../conversation",
            "/backend-api/codex/%2e%2e/conversation",
            "/backend-api/codex/%2E./conversation",
            "/backend-api/codex/./responses",
            "/backend-api/codex//responses",
            "/backend-api/codex/a%2f..%2fb",
            "/backend-api/codex/..%5cconversation",
            "/backend-api/codex/",
        ] {
            assert!(!is_codex(&request("POST", escape)), "{escape}");
        }
    }
}
