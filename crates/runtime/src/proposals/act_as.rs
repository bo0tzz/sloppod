//! Acting on a repository as the owner, with one of their GitHub tokens (ADR 28, ADR 29).
//!
//! A kind of its own, not a variant of `sloppod.repo.grant`, so a standing approval only covers it when it names it.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Manifest, ProposalKind, Result, SessionContext, ToolSpec, is_repo_name,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::{err, grant_repo_as, repo_granted_as, session_branch};
use crate::credentials::{CredentialRow, Credentials, token_reaches};
use crate::db;

pub struct ActAsOwner {
    db: PgPool,
    credentials: Arc<Credentials>,
}

impl ActAsOwner {
    pub fn new(db: PgPool, credentials: Arc<Credentials>) -> Self {
        Self { db, credentials }
    }

    /// The owner's token the proposal names, if it's still theirs and still reaches `repo`.
    async fn token(&self, owner: &str, payload: &Value) -> Result<(CredentialRow, String)> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let credential = payload["credential"].as_str().unwrap_or_default();
        let token = self
            .credentials
            .github_user(owner, credential)
            .await
            .map_err(err)?
            .ok_or_else(|| Error::new("that isn't one of the user's GitHub tokens"))?;
        let repo = token_reaches(&token, repo)
            .ok_or_else(|| Error::new(format!("the token {} doesn't reach {repo}", token.name)))?;
        Ok((token, repo))
    }
}

#[async_trait]
impl ProposalKind for ActAsOwner {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("github")
    }

    fn kind(&self) -> String {
        "sloppod.repo.act_as_owner".into()
    }

    fn target(&self, payload: &Value) -> Option<String> {
        payload["repo"].as_str().map(str::to_owned)
    }

    /// Approving picks which of the user's identities the session acts as (ADR 28, ADR 29), so a person always does.
    fn standing_approvable(&self) -> bool {
        false
    }

    fn summary(&self, payload: &Value) -> String {
        format!(
            "acting as {} on {}",
            payload["login"].as_str().unwrap_or("the user"),
            payload["repo"].as_str().unwrap_or_default()
        )
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "request_owner_identity".into(),
            description: "Ask to act on a GitHub repository as the user, with their own GitHub token, instead of \
                          sloppod's GitHub App: for a repository the App isn't installed on, or work GitHub only \
                          takes from a person. Git, GitHub's API and your proposals on it then go out in the user's \
                          name, so ask only when the App can't do the work. The user reviews it and picks which of \
                          their tokens; you're told when it's decided. You get write access to your session branch \
                          there too."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repository as owner/name"},
                    "reason": {
                        "type": "string",
                        "description": "Why the GitHub App can't do it, and what you need it for; shown to the user"
                    },
                },
                "required": ["repo", "reason"],
            }),
        }
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let wanted = arguments["repo"].as_str().unwrap_or_default().trim();
        if !is_repo_name(wanted) {
            return Err(Error::new("give the repository as owner/name"));
        }
        if let Some(login) = session.repo_acting_as(wanted) {
            return Err(Error::new(format!(
                "this session already acts as {login} on {wanted}"
            )));
        }
        // Never inherited, and never asked for by a sub-session (ADR 28): its parent asks.
        let id: Uuid = session.session_id.parse().map_err(err)?;
        if db::get_session(&self.db, id)
            .await
            .map_err(err)?
            .is_some_and(|row| row.parent_id.is_some())
        {
            return Err(Error::new(
                "a sub-session can't act as the user: say in your report that you need it, and your parent session \
                 can ask",
            ));
        }
        let tokens = self
            .credentials
            .github_users(&session.owner)
            .await
            .map_err(err)?;
        if tokens.is_empty() {
            return Err(Error::new(
                "the user has no GitHub token in sloppod to act with; tell them, if acting as them is what you need",
            ));
        }
        // The first token that reaches it; the user can pick another when approving.
        let Some((token, repo)) = tokens
            .iter()
            .find_map(|token| token_reaches(token, wanted).map(|repo| (token, repo)))
        else {
            return Err(Error::new(format!(
                "none of the user's GitHub tokens reach {wanted}; tell them, if acting as them is what you need"
            )));
        };
        Ok(json!({
            "repo": repo,
            "reason": arguments["reason"].as_str().unwrap_or_default(),
            "branch": session_branch(id),
            "credential": token.id.to_string(),
            "login": token.config("login").unwrap_or_default(),
        }))
    }

    async fn detail(&self, session: &SessionContext, _payload: &Value) -> Result<Value> {
        // The tokens the user can pick from, so a UI can offer them.
        let tokens = self
            .credentials
            .github_users(&session.owner)
            .await
            .map_err(err)?;
        let tokens: Vec<Value> = tokens
            .iter()
            .map(
                |token| json!({"id": token.id, "name": token.name, "login": token.config("login")}),
            )
            .collect();
        Ok(json!({"tokens": tokens}))
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        let mut edited = payload.clone();
        if let Some(credential) = edits.get("credential") {
            let credential = credential
                .as_str()
                .ok_or_else(|| Error::new("`credential` is the id of one of your GitHub tokens"))?;
            edited["credential"] = json!(credential);
            // Who it is follows the token; checked when it's carried out.
            edited.as_object_mut().map(|fields| fields.remove("login"));
        }
        Ok(edited)
    }

    async fn execute(
        &self,
        session: &SessionContext,
        _proposal: uuid::Uuid,
        payload: &Value,
    ) -> Result<Value> {
        let (token, repo) = self.token(&session.owner, payload).await?;
        grant_repo_as(&self.db, session, &repo, &token)
            .await
            .map_err(err)?;
        let session_id: Uuid = session.session_id.parse().map_err(err)?;
        // Who it ended up acting as, which an edit picking another token changed.
        Ok(json!({
            "message": repo_granted_as(&repo, &token, session_id),
            "login": token.config("login").unwrap_or_default(),
        }))
    }
}
