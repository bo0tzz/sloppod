use std::sync::Arc;

use async_trait::async_trait;
use sloppod_contracts::{
    Decision, Error, HttpRequest, Manifest, RequestPolicy, Result, SessionContext,
};

use crate::capabilities::{Part, Provider, holds};
use crate::credentials::{Account, CHATGPT, Credentials};

/// Codex's model API on the ChatGPT backend, reached with one of the owner's ChatGPT subscriptions (ADR 24).
const CODEX_PATH: &str = "/backend-api/codex/";

/// A codex grant whose login was deleted: it isn't replaced by another behind the owner's back (ADR 29).
const GONE: &str = "sloppod: the ChatGPT login this session's codex grant names was deleted; the user can switch the grant to another login in the session's grants list.\n";

/// Codex through a ChatGPT login: only Codex's own backend API, with the token and account of the login the session's
/// grant names (ADR 29) attached.
pub struct CodexPolicy {
    credentials: Arc<Credentials>,
}

impl CodexPolicy {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }
}

/// Only Codex's own API under `/backend-api/codex/`, read through the shared path parser: a path that could reach
/// another ChatGPT API with the owner's token is refused, whatever form it's written in.
fn is_codex(request: &HttpRequest) -> bool {
    let Ok(segments) = request.segments() else {
        return false;
    };
    matches!(request.method.as_str(), "GET" | "POST")
        && matches!(segments.as_slice(), [api, codex, _, ..] if api == "backend-api" && codex == "codex")
        // The listener resolves dot segments and the shared parser refuses them, but an escaped `/` or `\` is passed
        // on as sent, and ChatGPT's front end might split on it.
        && segments.iter().all(|segment| !segment.contains(['/', '\\']))
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
        if !holds(session, &Part::Model(Provider::ChatGpt)) {
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
        let login = match self
            .credentials
            .attach(session, CHATGPT)
            .await
            .map_err(|e| Error::new(e.to_string()))?
        {
            Account::Found(row) => row,
            Account::Gone => return Ok(Decision::deny(GONE)),
            Account::None => {
                return Ok(Decision::deny(format!(
                    "sloppod: {} has no ChatGPT login configured; add one in sloppod's settings.\n",
                    session.owner
                )));
            }
        };
        let Some(tokens) = self
            .credentials
            .chatgpt(&login)
            .await
            .map_err(|e| Error::new(e.to_string()))?
        else {
            return Ok(Decision::deny(GONE));
        };
        Ok(Decision::allow_with(
            vec![
                (
                    "authorization".into(),
                    format!("Bearer {}", tokens.access_token),
                ),
                ("chatgpt-account-id".into(), tokens.account_id),
            ],
            Vec::new(),
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
            "/backend-api/codex/a%2f..%2fb",
            "/backend-api/codex/..%5cconversation",
            "/backend-api/codex/",
        ] {
            assert!(!is_codex(&request("POST", escape)), "{escape}");
        }
    }
}
