//! Proposal kinds (ADR 3). The only one so far opens a pull request from the session branch.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{Error, Manifest, ProposalKind, Result, SessionContext, ToolSpec};
use sloppod_protocol::api::Proposal;
use uuid::Uuid;

use crate::credentials::Credentials;
use crate::github::GitHubApp;
use crate::{AppState, db};

#[derive(Default, Clone)]
pub struct ProposalKinds {
    by_kind: HashMap<String, Arc<dyn ProposalKind>>,
}

impl ProposalKinds {
    pub fn register(&mut self, kind: Arc<dyn ProposalKind>) {
        self.by_kind.insert(kind.kind(), kind);
    }

    pub fn get(&self, kind: &str) -> Option<&Arc<dyn ProposalKind>> {
        self.by_kind.get(kind)
    }

    pub fn by_tool(&self, tool: &str) -> Option<&Arc<dyn ProposalKind>> {
        self.by_kind.values().find(|k| k.tool().name == tool)
    }

    pub fn tools(&self) -> Vec<ToolSpec> {
        let mut tools: Vec<ToolSpec> = self.by_kind.values().map(|k| k.tool()).collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }
}

pub struct PullRequest {
    credentials: Arc<Credentials>,
}

impl PullRequest {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }

    /// The session owner's GitHub App, which both reads the comparison and opens the PR.
    async fn app(&self, session: &SessionContext) -> Result<Arc<GitHubApp>> {
        self.credentials.github(&session.owner).await.map_err(err)
    }
}

fn err(e: impl std::fmt::Display) -> Error {
    Error::new(e.to_string())
}

#[async_trait]
impl ProposalKind for PullRequest {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("github")
    }

    fn kind(&self) -> String {
        "github.pr.create".into()
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "propose_pull_request".into(),
            description: "Propose opening a pull request from your session branch. Push your commits to the \
                          session branch first. A human reviews the title, body and diff; once approved, \
                          sloppod opens the PR. Returns the proposal's id; check it with get_proposal."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Pull request title"},
                    "body": {"type": "string", "description": "Pull request description (Markdown)"},
                    "base": {"type": "string", "description": "Branch to merge into; defaults to the repository's default branch"},
                    "draft": {"type": "boolean", "description": "Open as a draft", "default": false},
                },
                "required": ["title", "body"],
            }),
        }
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let Some(grant) = session.writable_repo() else {
            return Err(Error::new(
                "this session has no repository, so there's nothing to open a pull request from",
            ));
        };
        let (repo, branch) = (grant.repo, grant.branch);
        let app = self.app(session).await?;
        let title = arguments["title"].as_str().filter(|t| !t.trim().is_empty());
        let Some(title) = title else {
            return Err(Error::new("a title is required"));
        };
        let base = match arguments["base"].as_str() {
            Some(base) => base.to_owned(),
            None => app.default_branch(repo).await.map_err(err)?,
        };
        let comparison = app.compare(repo, &base, branch).await.map_err(err)?;
        if comparison.ahead_by == 0 {
            return Err(Error::new(format!(
                "`{}` has no commits that aren't on `{base}`; commit and `git push origin HEAD:{}` first",
                branch, branch
            )));
        }
        Ok(json!({
            "repo": repo,
            "base": base,
            "head": branch,
            "title": title,
            "body": arguments["body"].as_str().unwrap_or_default(),
            "draft": arguments["draft"].as_bool().unwrap_or(false),
        }))
    }

    async fn detail(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let base = payload["base"].as_str().unwrap_or("main");
        let head = payload["head"].as_str().unwrap_or_default();
        let comparison = self
            .app(session)
            .await?
            .compare(payload["repo"].as_str().unwrap_or_default(), base, head)
            .await
            .map_err(err)?;
        Ok(json!({
            "diff": comparison.diff,
            "commits": comparison.commits.iter().map(|(sha, message)| json!({"sha": sha, "message": message})).collect::<Vec<_>>(),
        }))
    }

    async fn execute(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let request = json!({
            "title": payload["title"],
            "body": payload["body"],
            "head": payload["head"],
            "base": payload["base"],
            "draft": payload["draft"],
        });
        self.app(session)
            .await?
            .create_pull(payload["repo"].as_str().unwrap_or_default(), &request)
            .await
            .map_err(err)
    }
}

/// Creates a proposal from an agent's tool call.
pub async fn propose(
    state: &AppState,
    session_id: Uuid,
    tool: &str,
    arguments: Value,
) -> std::result::Result<Proposal, String> {
    let kind = state
        .proposals
        .by_tool(tool)
        .ok_or_else(|| format!("unknown tool {tool}"))?;
    let session = db::get_session(&state.db, session_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("unknown session")?;
    let context = session.context();
    let payload = kind.prepare(&context, arguments).await.map_err(|e| e.0)?;
    let row = db::insert_proposal(&state.db, session_id, &kind.kind(), &payload)
        .await
        .map_err(|e| e.to_string())?;
    let proposal = row.to_api();
    state.hub.broadcast_proposal(&proposal);
    Ok(proposal)
}

pub enum Verdict {
    Approve {
        title: Option<String>,
        body: Option<String>,
    },
    Reject {
        reason: Option<String>,
    },
}

/// Applies a human's decision; approving executes immediately (ADR 3).
/// A proposal, if its session belongs to `owner`.
async fn owned_proposal(
    state: &AppState,
    id: Uuid,
    owner: &str,
) -> anyhow::Result<Option<db::ProposalRow>> {
    let Some(row) = db::get_proposal(&state.db, id).await? else {
        return Ok(None);
    };
    Ok(db::owned_session(&state.db, row.session_id, owner)
        .await?
        .map(|_| row))
}

pub async fn decide(
    state: &AppState,
    id: Uuid,
    verdict: Verdict,
    owner: &str,
) -> anyhow::Result<Option<Proposal>> {
    let Some(row) = owned_proposal(state, id, owner).await? else {
        return Ok(None);
    };
    let mut payload = row.payload.clone();
    let (status, reason) = match verdict {
        Verdict::Approve { title, body } => {
            if let Some(title) = title {
                payload["title"] = json!(title);
            }
            if let Some(body) = body {
                payload["body"] = json!(body);
            }
            ("approved", None)
        }
        Verdict::Reject { reason } => ("rejected", reason),
    };
    let Some(row) =
        db::decide_proposal(&state.db, id, status, &payload, reason.as_deref(), owner).await?
    else {
        anyhow::bail!("proposal {id} was already decided");
    };
    state.hub.broadcast_proposal(&row.to_api());
    if status == "rejected" {
        return Ok(Some(row.to_api()));
    }
    let kind = state
        .proposals
        .get(&row.kind)
        .ok_or_else(|| anyhow::anyhow!("no executor for {}", row.kind))?;
    let session = db::get_session(&state.db, row.session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session gone"))?;
    let finished = match kind.execute(&session.context(), &payload).await {
        Ok(result) => db::finish_proposal(&state.db, id, "executed", Some(&result), None).await?,
        Err(e) => db::finish_proposal(&state.db, id, "failed", None, Some(&e.0)).await?,
    };
    let proposal = finished.to_api();
    state.hub.broadcast_proposal(&proposal);
    Ok(Some(proposal))
}

pub async fn detail(state: &AppState, id: Uuid, owner: &str) -> anyhow::Result<Option<Value>> {
    let Some(row) = owned_proposal(state, id, owner).await? else {
        return Ok(None);
    };
    let mut proposal = serde_json::to_value(row.to_api())?;
    let kind = state
        .proposals
        .get(&row.kind)
        .ok_or_else(|| anyhow::anyhow!("unknown kind"))?;
    let session = db::get_session(&state.db, row.session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session gone"))?;
    match kind.detail(&session.context(), &row.payload).await {
        Ok(Value::Object(extra)) => {
            for (k, v) in extra {
                proposal[k] = v;
            }
        }
        Ok(_) => {}
        Err(e) => {
            proposal["diff"] = json!("");
            proposal["commits"] = json!([]);
            proposal["detail_error"] = json!(e.0);
        }
    }
    Ok(Some(proposal))
}
