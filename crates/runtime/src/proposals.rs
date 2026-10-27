//! Proposal kinds (ADR 3): opening a pull request from the session branch, and write access to another repository.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Grant, Manifest, ProposalKind, RepoGrant, Result, SessionContext, ToolSpec,
};
use sloppod_protocol::api::Proposal;
use sqlx::PgPool;
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

/// The branch a session pushes to; the same in every repository it can write to.
pub fn session_branch(session: Uuid) -> String {
    format!("sloppod/{}", &session.simple().to_string()[..8])
}

/// Lets a session push its branch to `repo`, unless it already can; returns whether the grant is new.
pub async fn grant_repo(db: &PgPool, session: &SessionContext, repo: &str) -> anyhow::Result<bool> {
    if session.writable_repo_for(repo).is_some() {
        return Ok(false);
    }
    let branch = session_branch(session.session_id.parse()?);
    let grant = Grant::new(
        "repo",
        json!({"repo": repo, "access": "rw", "branch": branch}),
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
            description: "Propose opening a pull request from your session branch, or from a branch named after it \
                          (`<session branch>-<name>`) so several pull requests can be open at once. Push your \
                          commits first. A human reviews the title, body and diff; once approved, sloppod opens \
                          the PR. Returns the proposal's id; check it with get_proposal."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Pull request title"},
                    "body": {"type": "string", "description": "Pull request description (Markdown)"},
                    "repo": {"type": "string", "description": "Repository as owner/name; needed when the session can push to more than one"},
                    "base": {"type": "string", "description": "Branch to merge into; defaults to the repository's default branch"},
                    "head": {"type": "string", "description": "Branch to open it from: the session branch (the default) or `<session branch>-<name>`"},
                    "draft": {"type": "boolean", "description": "Open as a draft", "default": false},
                    "auto_merge": {"type": "boolean", "description": "Ask for the PR to merge by itself once its required checks pass; the reviewer decides", "default": false},
                },
                "required": ["title", "body"],
            }),
        }
    }

    fn summary(&self, payload: &Value) -> String {
        format!(
            "pull request \"{}\" on {}",
            payload["title"].as_str().unwrap_or_default(),
            payload["repo"].as_str().unwrap_or_default()
        )
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let RepoGrant { repo, branch } = pull_request_repo(session, arguments["repo"].as_str())?;
        let head = head_branch(branch, arguments["head"].as_str())?;
        let app = self.app(session).await?;
        let title = arguments["title"].as_str().filter(|t| !t.trim().is_empty());
        let Some(title) = title else {
            return Err(Error::new("a title is required"));
        };
        let base = match arguments["base"].as_str() {
            Some(base) => base.to_owned(),
            None => app.default_branch(repo).await.map_err(err)?,
        };
        let comparison = app.compare(repo, &base, head).await.map_err(err)?;
        if comparison.ahead_by == 0 {
            return Err(Error::new(format!(
                "`{head}` has no commits that aren't on `{base}`; commit and `git push origin HEAD:{head}` first"
            )));
        }
        Ok(json!({
            "repo": repo,
            "base": base,
            "head": head,
            "title": title,
            "body": arguments["body"].as_str().unwrap_or_default(),
            "draft": arguments["draft"].as_bool().unwrap_or(false),
            "auto_merge": arguments["auto_merge"].as_bool().unwrap_or(false),
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
        let repo = payload["repo"].as_str().unwrap_or_default();
        let app = self.app(session).await?;
        let mut result = app.create_pull(repo, &request).await.map_err(err)?;
        if payload["auto_merge"].as_bool() != Some(true) {
            return Ok(result);
        }
        let url = result["url"].as_str().unwrap_or_default().to_owned();
        let message = match app
            .enable_auto_merge(repo, result["node_id"].as_str().unwrap_or_default())
            .await
        {
            Ok(()) => {
                result["auto_merge"] = json!(true);
                format!("{url} is open and merges once its required checks pass.")
            }
            Err(e) => {
                result["auto_merge"] = json!(false);
                format!(
                    "{url} is open; auto-merge couldn't be turned on ({e}), so it needs merging by hand."
                )
            }
        };
        result["message"] = json!(message);
        Ok(result)
    }
}

/// Whether a session may push `branch`: its session branch, or one named after it (`<session branch>-<name>`).
/// Not `<session branch>/<name>`, which git can't have next to the session branch itself.
pub fn is_session_branch(session_branch: &str, branch: &str) -> bool {
    branch == session_branch
        || branch
            .strip_prefix(session_branch)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|name| !name.is_empty())
}

/// The branch a pull request is from.
fn head_branch<'a>(session_branch: &'a str, named: Option<&'a str>) -> Result<&'a str> {
    let Some(head) = named.map(str::trim).filter(|h| !h.is_empty()) else {
        return Ok(session_branch);
    };
    let head = head.strip_prefix("refs/heads/").unwrap_or(head);
    if is_session_branch(session_branch, head) {
        Ok(head)
    } else {
        Err(Error::new(format!(
            "pull requests come from your session branch `{session_branch}` or one named after it, like \
             `{session_branch}-<name>`"
        )))
    }
}

/// The repository a pull request is from: the one named, or the only one the session can push to.
fn pull_request_repo<'a>(
    session: &'a SessionContext,
    named: Option<&str>,
) -> Result<RepoGrant<'a>> {
    if let Some(repo) = named {
        return session.writable_repo_for(repo).ok_or_else(|| {
            Error::new(format!(
                "this session can't push to {repo}; ask for write access with request_repository first"
            ))
        });
    }
    let writable: Vec<RepoGrant> = session.writable_repos().collect();
    match writable.as_slice() {
        [] => Err(Error::new(
            "this session can't push to any repository; ask for write access with request_repository first",
        )),
        [only] => Ok(only.clone()),
        several => Err(Error::new(format!(
            "this session can push to {}; say which one with `repo`",
            several
                .iter()
                .map(|g| g.repo)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
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
        auto_merge: Option<bool>,
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
        Verdict::Approve {
            title,
            body,
            auto_merge,
        } => {
            if let Some(title) = title {
                payload["title"] = json!(title);
            }
            if let Some(body) = body {
                payload["body"] = json!(body);
            }
            if let Some(auto_merge) = auto_merge {
                payload["auto_merge"] = json!(auto_merge);
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
    let kind = state
        .proposals
        .get(&row.kind)
        .ok_or_else(|| anyhow::anyhow!("no executor for {}", row.kind))?;
    let summary = kind.summary(&payload);
    if status == "rejected" {
        let reason = row
            .reason
            .as_deref()
            .map(|r| format!(" Their reason: {r}"))
            .unwrap_or_default();
        state
            .hub
            .notify(
                row.session_id,
                format!("sloppod: the user rejected your proposal ({summary}).{reason}"),
            )
            .await;
        return Ok(Some(row.to_api()));
    }
    let session = db::get_session(&state.db, row.session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session gone"))?;
    let finished = match kind.execute(&session.context(), &payload).await {
        Ok(result) => db::finish_proposal(&state.db, id, "executed", Some(&result), None).await?,
        Err(e) => db::finish_proposal(&state.db, id, "failed", None, Some(&e.0)).await?,
    };
    let proposal = finished.to_api();
    state.hub.broadcast_proposal(&proposal);
    state.hub.broadcast_session(row.session_id).await;
    let outcome = match (&proposal.result, &proposal.error) {
        (Some(result), _) => match (result["message"].as_str(), result["url"].as_str()) {
            (Some(message), _) => format!("approved and done. {message}"),
            (None, Some(url)) => format!("approved and done: {url}"),
            (None, None) => "approved and done.".to_owned(),
        },
        (None, Some(error)) => format!("approved, but carrying it out failed: {error}"),
        (None, None) => "approved.".to_owned(),
    };
    state
        .hub
        .notify(
            row.session_id,
            format!("sloppod: your proposal ({summary}) was {outcome}"),
        )
        .await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session(repos: &[&str]) -> SessionContext {
        SessionContext {
            session_id: Uuid::nil().to_string(),
            owner: "dev".into(),
            grants: repos
                .iter()
                .map(|r| {
                    Grant::new(
                        "repo",
                        json!({"repo": r, "access": "rw", "branch": "sloppod/00000000"}),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn picks_the_pull_request_repository() {
        let one = session(&["a/one"]);
        assert_eq!(pull_request_repo(&one, None).unwrap().repo, "a/one");
        assert_eq!(
            pull_request_repo(&one, Some("A/One")).unwrap().repo,
            "a/one"
        );
        assert!(pull_request_repo(&one, Some("a/two")).is_err());
        assert!(pull_request_repo(&session(&[]), None).is_err());
        let two = session(&["a/one", "a/two"]);
        assert!(
            pull_request_repo(&two, None)
                .unwrap_err()
                .0
                .contains("a/one, a/two")
        );
        assert_eq!(
            pull_request_repo(&two, Some("a/two")).unwrap().repo,
            "a/two"
        );
    }

    #[test]
    fn picks_the_head_branch() {
        let branch = "sloppod/00000000";
        assert_eq!(head_branch(branch, None).unwrap(), branch);
        assert_eq!(head_branch(branch, Some(" ")).unwrap(), branch);
        assert_eq!(head_branch(branch, Some(branch)).unwrap(), branch);
        assert_eq!(
            head_branch(branch, Some("sloppod/00000000-issues")).unwrap(),
            "sloppod/00000000-issues"
        );
        assert_eq!(
            head_branch(branch, Some("refs/heads/sloppod/00000000-issues")).unwrap(),
            "sloppod/00000000-issues"
        );
        assert!(head_branch(branch, Some("sloppod/00000000-")).is_err());
        assert!(head_branch(branch, Some("sloppod/00000000/issues")).is_err());
        assert!(head_branch(branch, Some("sloppod/000000001")).is_err());
        assert!(head_branch(branch, Some("main")).is_err());
    }
}
