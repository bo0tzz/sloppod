use std::sync::Arc;

use async_trait::async_trait;
use sloppod_contracts::{
    Decision, Error, HttpRequest, Manifest, RequestPolicy, Result, SessionContext,
};

use crate::capabilities::{Part, Provider, holds};
use crate::credentials::{Account, CLAUDE, Credentials};

const INFERENCE: &[&str] = &["/v1/messages", "/v1/messages/count_tokens"];

/// A model grant whose account was deleted: it isn't replaced by another behind the owner's back (ADR 29).
const GONE: &str = "sloppod: the Claude account this session's model grant names was deleted; the user can switch the grant to another account in the session's grants list.\n";

/// The Anthropic API: inference only, with the Claude account the session's grant names (ADR 11 spike results, ADR 29).
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
        if !holds(session, &Part::Model(Provider::Anthropic)) {
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
        let account = match self
            .credentials
            .attach(session, CLAUDE)
            .await
            .map_err(|e| Error::new(e.to_string()))?
        {
            Account::Found(row) => row,
            Account::Gone => return Ok(Decision::deny(GONE)),
            Account::None => {
                return Ok(Decision::deny(format!(
                    "sloppod: {} has no Claude account configured; add one in sloppod's settings.\n",
                    session.owner
                )));
            }
        };
        let token = self
            .credentials
            .reveal(&account)
            .map_err(|e| Error::new(e.to_string()))?;
        Ok(Decision::allow_with(
            vec![("authorization".into(), format!("Bearer {token}"))],
            Vec::new(),
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

    #[sqlx::test]
    async fn attaches_the_account_the_grant_names_and_refuses_one_deleted(db: sqlx::PgPool) {
        use base64::Engine;
        use sloppod_contracts::Grant;
        use sloppod_protocol::api::NewCredential;
        let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        let credentials = Arc::new(
            Credentials::new(db, crate::vault::Vault::from_base64(&key).unwrap())
                .await
                .unwrap(),
        );
        let add = async |name: &str| {
            let new = NewCredential {
                kind: CLAUDE.into(),
                name: name.into(),
                config: Default::default(),
                secret: format!("sk-{name}"),
            };
            credentials.create("dev", new).await.unwrap().unwrap().id
        };
        let _default = add("personal").await;
        let work = add("work").await;
        let policy = ModelPolicy::new(credentials.clone());
        let session = SessionContext {
            session_id: "s".into(),
            owner: "dev".into(),
            grants: vec![Grant::new(
                "model",
                serde_json::json!({"credentials": [work.to_string()]}),
            )],
            role: Default::default(),
        };
        let decision = policy
            .decide(&session, &request("POST", "/v1/messages"))
            .await
            .unwrap();
        assert_eq!(
            decision,
            Decision::allow_with(
                vec![("authorization".into(), "Bearer sk-work".into())],
                Vec::new()
            )
        );
        credentials.delete("dev", work).await.unwrap();
        let decision = policy
            .decide(&session, &request("POST", "/v1/messages"))
            .await
            .unwrap();
        assert!(
            matches!(&decision, Decision::Deny { message, .. } if message.contains("was deleted")),
            "{decision:?}"
        );
    }
}
