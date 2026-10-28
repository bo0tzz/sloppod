//! Write access to another repository (ADR 3).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::capability;
use sloppod_contracts::{
    Error, Grant, Manifest, ProposalKind, READ_WRITE, Result, SessionContext, ToolSpec,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::{err, session_branch};
use crate::credentials::Credentials;
use crate::db;

/// Lets a session push its branch to `repo`, unless it already can; returns whether the grant is new.
pub async fn grant_repo(db: &PgPool, session: &SessionContext, repo: &str) -> anyhow::Result<bool> {
    if session.writable_repo_for(repo).is_some() {
        return Ok(false);
    }
    let branch = session_branch(session.session_id.parse()?);
    let grant = Grant::new(
        capability::REPO,
        json!({"repo": repo, "access": READ_WRITE, "branch": branch}),
    );
    db::add_grant(db, session.session_id.parse()?, &grant).await?;
    Ok(true)
}

/// What the agent needs to know to use a repository it was just given.
pub fn repo_granted(repo: &str, session: Uuid) -> String {
    let branch = session_branch(session);
    format!(
        "You can now push to {repo}. Clone it with `git clone https://github.com/{repo} {}/{repo}` if you haven't, \
         work on your session branch (`git switch -c {branch}`) and push with `git push origin HEAD:{branch}`; \
         only it and branches named after it (`{branch}-<name>`) can be pushed there.",
        crate::link::REPOS_DIR
    )
}

pub struct RepoAccess {
    db: PgPool,
    credentials: Arc<Credentials>,
}

impl RepoAccess {
    pub fn new(db: PgPool, credentials: Arc<Credentials>) -> Self {
        Self { db, credentials }
    }
}

#[async_trait]
impl ProposalKind for RepoAccess {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("github")
    }

    fn kind(&self) -> String {
        "sloppod.repo.grant".into()
    }

    fn target(&self, payload: &Value) -> Option<String> {
        payload["repo"].as_str().map(str::to_owned)
    }

    fn summary(&self, payload: &Value) -> String {
        format!(
            "write access to {}",
            payload["repo"].as_str().unwrap_or_default()
        )
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "request_repository".into(),
            description: "Ask for write access to a GitHub repository, so you can push your session branch to it \
                          and propose pull requests from it. A human reviews the request; you're told when it's \
                          decided. Reading and cloning repositories doesn't need this."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repository as owner/name"},
                    "reason": {"type": "string", "description": "What you need it for, shown to the reviewer"},
                },
                "required": ["repo", "reason"],
            }),
        }
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let wanted = arguments["repo"].as_str().unwrap_or_default().trim();
        if wanted.split('/').count() != 2 {
            return Err(Error::new("give the repository as owner/name"));
        }
        if session.writable_repo_for(wanted).is_some() {
            return Err(Error::new(format!(
                "this session can already push to {wanted}"
            )));
        }
        let repos = self
            .credentials
            .github(&session.owner)
            .await
            .map_err(err)?
            .repos()
            .await
            .map_err(err)?;
        let Some(repo) = repos.into_iter().find(|r| r.eq_ignore_ascii_case(wanted)) else {
            return Err(Error::new(format!(
                "sloppod's GitHub App isn't installed on {wanted}, so it can't be granted"
            )));
        };
        let session_id: Uuid = session.session_id.parse().map_err(err)?;
        Ok(json!({
            "repo": repo,
            "reason": arguments["reason"].as_str().unwrap_or_default(),
            "branch": session_branch(session_id),
        }))
    }

    async fn detail(&self, _session: &SessionContext, _payload: &Value) -> Result<Value> {
        Ok(json!({}))
    }

    async fn execute(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        grant_repo(&self.db, session, repo).await.map_err(err)?;
        let session_id: Uuid = session.session_id.parse().map_err(err)?;
        Ok(json!({"message": repo_granted(repo, session_id)}))
    }
}
