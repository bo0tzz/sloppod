//! Proposal kinds (ADR 3): opening a pull request from the session branch, and write access to another repository.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Grant, Manifest, ProposalKind, RepoGrant, Result, ReviewSubject, SessionContext,
    ToolSpec,
};
use sloppod_protocol::api::Proposal;
use sqlx::PgPool;
use uuid::Uuid;

use crate::credentials::Credentials;
use crate::github::GitHubApp;
use crate::{AppState, db, reviews};

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
                    "review": {"type": "boolean", "description": "Have another model review it before a human sees it; its findings come back to you", "default": false},
                },
                "required": ["title", "body"],
            }),
        }
    }

    fn target(&self, payload: &Value) -> Option<String> {
        payload["repo"].as_str().map(str::to_owned)
    }

    fn review_subject(&self, payload: &Value) -> Option<ReviewSubject> {
        let field = |key: &str| payload[key].as_str().map(str::to_owned);
        Some(ReviewSubject {
            repo: field("repo")?,
            base: field("base")?,
            head: field("head")?,
            head_sha: field("head_sha")?,
            title: field("title")?,
            body: field("body").unwrap_or_default(),
        })
    }

    fn summary(&self, payload: &Value) -> String {
        format!(
            "pull request \"{}\" on {}",
            payload["title"].as_str().unwrap_or_default(),
            payload["repo"].as_str().unwrap_or_default()
        )
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let RepoGrant { repo, branch } = target_repo(session, arguments["repo"].as_str())?;
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
            "head_sha": comparison.head_sha,
            "title": title,
            "body": arguments["body"].as_str().unwrap_or_default(),
            "draft": arguments["draft"].as_bool().unwrap_or(false),
            "auto_merge": arguments["auto_merge"].as_bool().unwrap_or(false),
        }))
    }

    async fn detail(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let base = payload["base"].as_str().unwrap_or("main");
        // What the reviewer sees is the commit that was proposed, not wherever the branch is now.
        let head = payload["head_sha"]
            .as_str()
            .or(payload["head"].as_str())
            .unwrap_or_default();
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

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        edit_pull_request(payload, edits)
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
        let head = payload["head"].as_str().unwrap_or_default();
        let app = self.app(session).await?;
        let reviewed = payload["head_sha"].as_str().unwrap_or_default();
        let current = app.branch_sha(repo, head).await.map_err(err)?;
        if current != reviewed {
            return Err(Error::new(format!(
                "`{head}` has moved since it was proposed (from {reviewed:.7} to {current:.7}), so the pull request \
                 wasn't opened; propose it again so the new commits get reviewed"
            )));
        }
        // A runtime restart can carry out an approved proposal again; reuse the pull request it already opened.
        let existing = app
            .open_pull(repo, head, payload["base"].as_str().unwrap_or_default())
            .await
            .map_err(err)?;
        let mut result = match existing {
            Some(pull) => pull,
            None => app.create_pull(repo, &request).await.map_err(err)?,
        };
        if payload["auto_merge"].as_bool() != Some(true) {
            return Ok(result);
        }
        let url = result["url"].as_str().unwrap_or_default().to_owned();
        let message = match app
            .enable_auto_merge(
                repo,
                result["node_id"].as_str().unwrap_or_default(),
                reviewed,
            )
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

/// Several pull requests that build on each other, opened together and linked as a stack on GitHub.
pub struct Stack {
    credentials: Arc<Credentials>,
}

impl Stack {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }

    async fn app(&self, session: &SessionContext) -> Result<Arc<GitHubApp>> {
        self.credentials.github(&session.owner).await.map_err(err)
    }
}

fn stack_layers(payload: &Value) -> &[Value] {
    payload["pull_requests"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// The whole stack at once: from its base to the top branch, with every layer's title and description.
fn stack_review_subject(payload: &Value) -> Option<ReviewSubject> {
    let layers = stack_layers(payload);
    let top = layers.last()?;
    let body = layers
        .iter()
        .enumerate()
        .map(|(i, l)| {
            format!(
                "## {}. {} (`{}` into `{}`)\n\n{}",
                i + 1,
                l["title"].as_str().unwrap_or_default(),
                l["head"].as_str().unwrap_or_default(),
                l["base"].as_str().unwrap_or_default(),
                l["body"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(ReviewSubject {
        repo: payload["repo"].as_str()?.to_owned(),
        base: payload["base"].as_str()?.to_owned(),
        head: top["head"].as_str()?.to_owned(),
        head_sha: top["head_sha"].as_str()?.to_owned(),
        title: format!("a stack of {} pull requests", layers.len()),
        body,
    })
}

/// Applies edits `{pull_requests: [{title?, body?}]}`, by position, to a stack's pull requests.
fn edit_stack(payload: &Value, edits: &Value) -> Result<Value> {
    let mut payload = payload.clone();
    if let Some(value) = edits.get("auto_merge") {
        let auto_merge = value
            .as_bool()
            .ok_or_else(|| Error::new("`auto_merge` must be true or false"))?;
        payload["auto_merge"] = json!(auto_merge);
    }
    let Some(edits) = edits.get("pull_requests") else {
        return Ok(payload);
    };
    let edits = edits
        .as_array()
        .ok_or_else(|| Error::new("`pull_requests` must be a list"))?;
    let layers = payload["pull_requests"]
        .as_array_mut()
        .ok_or_else(|| Error::new("the stack has no pull requests"))?;
    if edits.len() > layers.len() {
        return Err(Error::new("more edits than pull requests in the stack"));
    }
    for (layer, edit) in layers.iter_mut().zip(edits) {
        *layer = edit_pull_request(layer, edit)?;
    }
    Ok(payload)
}

#[async_trait]
impl ProposalKind for Stack {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("github")
    }

    fn kind(&self) -> String {
        "github.stack.create".into()
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "propose_stack".into(),
            description: "Propose a stack of pull requests that build on each other, e.g. branches managed with \
                          `gh stack`: each one merges into the branch below it, the bottom one into the base. \
                          Push every branch first; they come from your session branch or branches named after it. \
                          A human reviews them together; once approved, sloppod opens them and links them as a \
                          stack on GitHub. Returns the proposal's id; check it with get_proposal."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repository as owner/name; needed when the session can push to more than one"},
                    "base": {"type": "string", "description": "Branch the bottom pull request merges into; defaults to the repository's default branch"},
                    "pull_requests": {
                        "type": "array",
                        "description": "The stack, bottom first; at least two",
                        "minItems": 2,
                        "items": {
                            "type": "object",
                            "properties": {
                                "head": {"type": "string", "description": "Branch: the session branch or `<session branch>-<name>`"},
                                "title": {"type": "string"},
                                "body": {"type": "string", "description": "Description (Markdown)"},
                                "draft": {"type": "boolean", "default": false},
                            },
                            "required": ["head", "title", "body"],
                        },
                    },
                    "auto_merge": {"type": "boolean", "description": "Ask for the whole stack to merge by itself once its required checks pass; the reviewer decides", "default": false},
                    "review": {"type": "boolean", "description": "Have another model review the stack before a human sees it; its findings come back to you", "default": false},
                },
                "required": ["pull_requests"],
            }),
        }
    }

    fn target(&self, payload: &Value) -> Option<String> {
        payload["repo"].as_str().map(str::to_owned)
    }

    fn review_subject(&self, payload: &Value) -> Option<ReviewSubject> {
        stack_review_subject(payload)
    }

    fn summary(&self, payload: &Value) -> String {
        let layers = stack_layers(payload);
        format!(
            "stack of {} pull requests on {}, from \"{}\"",
            layers.len(),
            payload["repo"].as_str().unwrap_or_default(),
            layers
                .first()
                .and_then(|l| l["title"].as_str())
                .unwrap_or_default()
        )
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let RepoGrant { repo, branch } = target_repo(session, arguments["repo"].as_str())?;
        let requested = stack_layers(&arguments);
        if requested.len() < 2 {
            return Err(Error::new(
                "a stack has at least two pull requests; propose a single one with propose_pull_request",
            ));
        }
        let app = self.app(session).await?;
        let mut base = match arguments["base"].as_str() {
            Some(base) => base.to_owned(),
            None => app.default_branch(repo).await.map_err(err)?,
        };
        let bottom = base.clone();
        let mut layers = Vec::new();
        for requested in requested {
            let head = head_branch(branch, requested["head"].as_str())?.to_owned();
            if head == base || layers.iter().any(|l: &Value| l["head"] == head.as_str()) {
                return Err(Error::new(format!("`{head}` is in the stack twice")));
            }
            let comparison = app.compare(repo, &base, &head).await.map_err(err)?;
            if comparison.ahead_by == 0 {
                return Err(Error::new(format!(
                    "`{head}` has no commits that aren't on `{base}`; commit and push it first"
                )));
            }
            let layer = edit_pull_request(
                &json!({
                    "head": head,
                    "base": base,
                    "head_sha": comparison.head_sha,
                    "draft": requested["draft"].as_bool().unwrap_or(false),
                }),
                &json!({"title": requested["title"], "body": requested["body"].as_str().unwrap_or_default()}),
            )?;
            base = head;
            layers.push(layer);
        }
        Ok(json!({
            "repo": repo,
            "base": bottom,
            "pull_requests": layers,
            "auto_merge": arguments["auto_merge"].as_bool().unwrap_or(false),
        }))
    }

    async fn detail(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let app = self.app(session).await?;
        let repo = payload["repo"].as_str().unwrap_or_default();
        let mut layers = Vec::new();
        for layer in stack_layers(payload) {
            let comparison = app
                .compare(
                    repo,
                    layer["base"].as_str().unwrap_or_default(),
                    layer["head_sha"].as_str().unwrap_or_default(),
                )
                .await
                .map_err(err)?;
            layers.push(json!({
                "diff": comparison.diff,
                "commits": comparison.commits.iter().map(|(sha, message)| json!({"sha": sha, "message": message})).collect::<Vec<_>>(),
            }));
        }
        Ok(json!({"layers": layers}))
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        edit_stack(payload, edits)
    }

    async fn execute(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let app = self.app(session).await?;
        let layers = stack_layers(payload);
        // Nothing is opened unless every branch is still at the commit that was reviewed.
        for layer in layers {
            let head = layer["head"].as_str().unwrap_or_default();
            let reviewed = layer["head_sha"].as_str().unwrap_or_default();
            let current = app.branch_sha(repo, head).await.map_err(err)?;
            if current != reviewed {
                return Err(Error::new(format!(
                    "`{head}` has moved since it was proposed (from {reviewed:.7} to {current:.7}), so nothing \
                     was opened; propose the stack again so the new commits get reviewed"
                )));
            }
        }
        let mut pulls = Vec::new();
        for layer in layers {
            let head = layer["head"].as_str().unwrap_or_default();
            let base = layer["base"].as_str().unwrap_or_default();
            // A runtime restart can carry out an approved proposal again; reuse what it already opened.
            let pull = match app.open_pull(repo, head, base).await.map_err(err)? {
                Some(pull) => pull,
                None => app
                    .create_pull(
                        repo,
                        &json!({
                            "title": layer["title"],
                            "body": layer["body"],
                            "head": head,
                            "base": base,
                            "draft": layer["draft"],
                        }),
                    )
                    .await
                    .map_err(err)?,
            };
            pulls.push(pull);
        }
        let numbers: Vec<u64> = pulls.iter().filter_map(|p| p["number"].as_u64()).collect();
        let urls: Vec<&str> = pulls.iter().filter_map(|p| p["url"].as_str()).collect();
        // An earlier attempt may have linked them already.
        let top = numbers.last().copied().unwrap_or_default();
        let linked = match app.stack_of(repo, top).await {
            Ok(Some(stack)) => Ok(stack),
            Ok(None) => app.link_stack(repo, &numbers).await,
            Err(e) => Err(e),
        };
        let mut message = match &linked {
            Ok(stack) => format!("Opened {} as stack #{stack}.", urls.join(", ")),
            Err(e) => format!(
                "Opened {}, but linking them as a stack failed ({e}); each one still merges into the one below.",
                urls.join(", ")
            ),
        };
        let mut auto_merge = false;
        if payload["auto_merge"].as_bool() == Some(true) {
            // Auto-merge on the top pull request merges the whole stack, so only a linked stack gets it.
            let (top, top_layer) = (pulls.last(), layers.last());
            message.push_str(&match (&linked, top, top_layer) {
                (Ok(stack), Some(top), Some(layer)) => match app
                    .auto_merge_stack(
                        repo,
                        *stack,
                        &numbers,
                        top["node_id"].as_str().unwrap_or_default(),
                        layer["head_sha"].as_str().unwrap_or_default(),
                    )
                    .await
                {
                    Ok(()) => {
                        auto_merge = true;
                        " The stack merges once its required checks pass.".to_owned()
                    }
                    Err(e) => format!(
                        " Auto-merge couldn't be turned on ({e}), so it needs merging by hand."
                    ),
                },
                _ => " Without a linked stack, auto-merge stays off.".to_owned(),
            });
        }
        Ok(json!({
            "pull_requests": pulls,
            "stack": linked.ok(),
            "auto_merge": auto_merge,
            "message": message,
        }))
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

fn edit_pull_request(payload: &Value, edits: &Value) -> Result<Value> {
    let mut payload = payload.clone();
    for field in ["title", "body"] {
        if let Some(value) = edits.get(field) {
            let text = value
                .as_str()
                .ok_or_else(|| Error::new(format!("`{field}` must be a string")))?;
            payload[field] = json!(text);
        }
    }
    if let Some(value) = edits.get("auto_merge") {
        let auto_merge = value
            .as_bool()
            .ok_or_else(|| Error::new("`auto_merge` must be true or false"))?;
        payload["auto_merge"] = json!(auto_merge);
    }
    if payload["title"]
        .as_str()
        .is_none_or(|title| title.trim().is_empty())
    {
        return Err(Error::new("a title is required"));
    }
    Ok(payload)
}

/// The repository a pull request or issue is for: the one named, or the only one the session can push to.
fn target_repo<'a>(session: &'a SessionContext, named: Option<&str>) -> Result<RepoGrant<'a>> {
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

pub struct Issues {
    credentials: Arc<Credentials>,
}

impl Issues {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }
}

const MAX_ISSUES: usize = 50;

/// Checks a list of issues from the agent or the reviewer, keeping only the fields GitHub gets.
fn issue_list(issues: &Value) -> Result<Vec<Value>> {
    let Some(issues) = issues.as_array().filter(|i| !i.is_empty()) else {
        return Err(Error::new("give at least one issue"));
    };
    if issues.len() > MAX_ISSUES {
        return Err(Error::new(format!(
            "propose at most {MAX_ISSUES} issues at a time"
        )));
    }
    issues
        .iter()
        .enumerate()
        .map(|(i, issue)| {
            let title = issue["title"].as_str().map(str::trim).unwrap_or_default();
            if title.is_empty() {
                return Err(Error::new(format!("issue {} has no title", i + 1)));
            }
            let labels: Vec<&str> = match &issue["labels"] {
                Value::Null => Vec::new(),
                Value::Array(labels) => labels
                    .iter()
                    .map(|l| l.as_str().map(str::trim).filter(|l| !l.is_empty()))
                    .collect::<Option<_>>()
                    .ok_or_else(|| Error::new(format!("issue {}'s labels must be names", i + 1)))?,
                _ => {
                    return Err(Error::new(format!(
                        "issue {}'s labels must be a list",
                        i + 1
                    )));
                }
            };
            Ok(json!({
                "title": title,
                "body": issue["body"].as_str().unwrap_or_default(),
                "labels": labels,
            }))
        })
        .collect()
}

/// The reviewer sends the issues to file, so dropping one leaves it out.
fn edit_issues(payload: &Value, edits: &Value) -> Result<Value> {
    let mut payload = payload.clone();
    if let Some(issues) = edits.get("issues") {
        payload["issues"] = json!(issue_list(issues)?);
    }
    Ok(payload)
}

#[async_trait]
impl ProposalKind for Issues {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("github")
    }

    fn kind(&self) -> String {
        "github.issue.create".into()
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "propose_issues".into(),
            description: "Propose filing one or more GitHub issues in a repository this session can push to. A \
                          human reviews them together, and can edit or drop any of them; once approved, sloppod \
                          files them, creating labels that don't exist yet. Returns the proposal's id; check it \
                          with get_proposal."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repository as owner/name; needed when the session can push to more than one"},
                    "issues": {
                        "type": "array",
                        "maxItems": MAX_ISSUES,
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "body": {"type": "string", "description": "Markdown"},
                                "labels": {"type": "array", "items": {"type": "string"}},
                            },
                            "required": ["title"],
                        },
                    },
                },
                "required": ["issues"],
            }),
        }
    }

    fn target(&self, payload: &Value) -> Option<String> {
        payload["repo"].as_str().map(str::to_owned)
    }

    fn summary(&self, payload: &Value) -> String {
        let repo = payload["repo"].as_str().unwrap_or_default();
        match payload["issues"].as_array().map(Vec::as_slice) {
            Some([issue]) => format!(
                "issue \"{}\" on {repo}",
                issue["title"].as_str().unwrap_or_default()
            ),
            Some(issues) => format!("{} issues on {repo}", issues.len()),
            None => format!("issues on {repo}"),
        }
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let RepoGrant { repo, .. } = target_repo(session, arguments["repo"].as_str())?;
        Ok(json!({"repo": repo, "issues": issue_list(&arguments["issues"])?}))
    }

    async fn detail(&self, _session: &SessionContext, _payload: &Value) -> Result<Value> {
        Ok(json!({}))
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        edit_issues(payload, edits)
    }

    async fn execute(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let issues = issue_list(&payload["issues"])?;
        let app = self.credentials.github(&session.owner).await.map_err(err)?;
        let mut labels: Vec<&str> = issues
            .iter()
            .flat_map(|i| i["labels"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .collect();
        labels.sort_unstable();
        labels.dedup();
        app.ensure_labels(repo, &labels).await.map_err(err)?;

        let (mut filed, mut failed) = (Vec::new(), Vec::new());
        // A runtime restart can carry out an approved proposal again; don't file the same issue twice.
        let recent = app.recent_issues(repo).await.map_err(err)?;
        for issue in &issues {
            if let Some(existing) = issue["title"].as_str().and_then(|t| recent.get(t)) {
                filed.push(json!({"title": issue["title"], "url": existing["url"], "number": existing["number"]}));
                continue;
            }
            match app.create_issue(repo, issue).await {
                Ok(result) => filed.push(json!({
                    "title": issue["title"],
                    "url": result["url"],
                    "number": result["number"],
                })),
                Err(e) => failed.push(json!({"title": issue["title"], "error": e.to_string()})),
            }
        }
        if filed.is_empty() {
            return Err(Error::new(format!(
                "no issues were filed: {}",
                failed[0]["error"].as_str().unwrap_or_default()
            )));
        }
        let mut message = format!(
            "Filed {}.",
            filed
                .iter()
                .map(|i| format!("#{} {}", i["number"], i["url"].as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for failure in &failed {
            message.push_str(&format!(
                " \"{}\" failed: {}.",
                failure["title"].as_str().unwrap_or_default(),
                failure["error"].as_str().unwrap_or_default()
            ));
        }
        Ok(json!({"issues": filed, "failed": failed, "message": message}))
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

/// Creates a proposal from an agent's tool call. With `review`, another model reviews it first (ADR 24).
pub async fn propose(
    state: &AppState,
    session_id: Uuid,
    tool: &str,
    arguments: Value,
) -> std::result::Result<Proposal, String> {
    let kind = state
        .proposals
        .by_tool(tool)
        .ok_or_else(|| format!("unknown tool {tool}"))?
        .clone();
    let session = db::get_session(&state.db, session_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("unknown session")?;
    // The session's policy covers every kind that can be reviewed; asking for a review of one that can't is an
    // error, as is asking when the owner has no login for the reviewer. Under the policy those just skip review.
    let asked = arguments["review"].as_bool() == Some(true);
    let required = session.review == "required";
    let context = session.context();
    let payload = kind.prepare(&context, arguments).await.map_err(|e| e.0)?;
    let mut subject = kind.review_subject(&payload).filter(|_| asked || required);
    if asked && subject.is_none() {
        return Err("proposals of this kind aren't reviewed; propose it without review".into());
    }
    if subject.is_some()
        && !reviews::available(state, &session.owner)
            .await
            .map_err(|e| e.to_string())?
    {
        if asked {
            return Err(
                "reviews need a ChatGPT login in sloppod's settings, which the user hasn't added; \
                        propose it without review"
                    .into(),
            );
        }
        subject = None;
    }
    let status = if subject.is_some() {
        "in_review"
    } else {
        "pending"
    };
    let row = db::insert_proposal(&state.db, session_id, &kind.kind(), status, &payload)
        .await
        .map_err(|e| e.to_string())?;
    let proposal = row.to_api();
    state.hub.broadcast_proposal(&proposal);
    let Some(subject) = subject else {
        return apply_standing_approval(state, &session, kind.as_ref(), proposal)
            .await
            .map_err(|e| e.to_string());
    };
    if let Err(e) = reviews::start(state, &row, &subject, None).await {
        tracing::warn!(proposal = %row.id, error = %e, "starting a review");
        let row = db::move_proposal(&state.db, row.id, "in_review", "pending", None)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("the proposal changed meanwhile")?;
        state.hub.broadcast_proposal(&row.to_api());
        return Err(format!(
            "the review couldn't start ({e}), so proposal {} went to the user without one",
            row.id
        ));
    }
    Ok(proposal)
}

/// Approves a proposal that's waiting for a human when one of the owner's standing approvals covers it.
pub async fn apply_standing_approval(
    state: &AppState,
    session: &db::SessionRow,
    kind: &dyn ProposalKind,
    proposal: Proposal,
) -> anyhow::Result<Proposal> {
    let standing = db::matching_standing_approval(
        &state.db,
        &session.owner,
        &kind.kind(),
        kind.target(&proposal.payload).as_deref(),
        session.id,
    )
    .await?;
    let Some(standing) = standing else {
        return Ok(proposal);
    };
    let decided_by = format!("standing-approval:{}", standing.id);
    match decide(
        state,
        proposal.id,
        Verdict::Approve {
            edits: standing.edits,
        },
        &session.owner,
        &decided_by,
    )
    .await
    {
        Ok(Some(approved)) => Ok(approved),
        // Left for a human to review, like any other proposal.
        Ok(None) | Err(_) => Ok(proposal),
    }
}

pub enum Verdict {
    Approve { edits: Value },
    Reject { reason: Option<String> },
}

/// Applies a human's decision; approving executes immediately (ADR 3).
/// A proposal, if its session belongs to `owner`.
pub async fn owned_proposal(
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

/// Records a decision on one of `owner`'s proposals, made by `decided_by` (the user, or a standing approval).
pub async fn decide(
    state: &AppState,
    id: Uuid,
    verdict: Verdict,
    owner: &str,
    decided_by: &str,
) -> anyhow::Result<Option<Proposal>> {
    let Some(row) = owned_proposal(state, id, owner).await? else {
        return Ok(None);
    };
    let kind = state
        .proposals
        .get(&row.kind)
        .ok_or_else(|| anyhow::anyhow!("no executor for {}", row.kind))?;
    let (status, payload, reason) = match verdict {
        Verdict::Approve { edits } => (
            "approved",
            kind.edit(&row.payload, &edits)
                .map_err(|e| anyhow::anyhow!(e.0))?,
            None,
        ),
        Verdict::Reject { reason } => ("rejected", row.payload.clone(), reason),
    };
    let Some(row) = db::decide_proposal(
        &state.db,
        id,
        status,
        &payload,
        reason.as_deref(),
        decided_by,
    )
    .await?
    else {
        anyhow::bail!("proposal {id} was already decided");
    };
    state.hub.broadcast_proposal(&row.to_api());
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
    // Carrying it out can take a while (GitHub calls), so the approval returns now and the outcome follows as a
    // proposal update.
    let (state, kind, session_id) = (state.clone(), kind.clone(), row.session_id);
    tokio::spawn(async move {
        if let Err(e) = carry_out(&state, id, session_id, kind.as_ref(), &payload, &summary).await {
            tracing::error!(proposal = %id, error = %e, "carrying out a proposal");
        }
    });
    Ok(Some(row.to_api()))
}

/// Carries out proposals that were approved but never finished, e.g. when the runtime restarted in between.
pub async fn resume_approved(state: &AppState) {
    let rows = match db::approved_proposals(&state.db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "listing approved proposals");
            return;
        }
    };
    for row in rows {
        let Some(kind) = state.proposals.get(&row.kind).cloned() else {
            continue;
        };
        tracing::info!(proposal = %row.id, kind = %row.kind, "resuming an approved proposal");
        let summary = kind.summary(&row.payload);
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = carry_out(
                &state,
                row.id,
                row.session_id,
                kind.as_ref(),
                &row.payload,
                &summary,
            )
            .await
            {
                tracing::error!(proposal = %row.id, error = %e, "carrying out a resumed proposal");
            }
        });
    }
}

async fn carry_out(
    state: &AppState,
    id: Uuid,
    session_id: Uuid,
    kind: &dyn ProposalKind,
    payload: &Value,
    summary: &str,
) -> anyhow::Result<()> {
    let session = db::get_session(&state.db, session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session gone"))?;
    let finished = match kind.execute(&session.context(), payload).await {
        Ok(result) => db::finish_proposal(&state.db, id, "executed", Some(&result), None).await?,
        Err(e) => db::finish_proposal(&state.db, id, "failed", None, Some(&e.0)).await?,
    };
    let proposal = finished.to_api();
    state.hub.broadcast_proposal(&proposal);
    state.hub.broadcast_session(session_id).await;
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
            session_id,
            format!("sloppod: your proposal ({summary}) was {outcome}"),
        )
        .await;
    Ok(())
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
    fn picks_the_target_repository() {
        let one = session(&["a/one"]);
        assert_eq!(target_repo(&one, None).unwrap().repo, "a/one");
        assert_eq!(target_repo(&one, Some("A/One")).unwrap().repo, "a/one");
        assert!(target_repo(&one, Some("a/two")).is_err());
        assert!(target_repo(&session(&[]), None).is_err());
        let two = session(&["a/one", "a/two"]);
        assert!(
            target_repo(&two, None)
                .unwrap_err()
                .0
                .contains("a/one, a/two")
        );
        assert_eq!(target_repo(&two, Some("a/two")).unwrap().repo, "a/two");
    }

    #[test]
    fn checks_issue_lists() {
        let issues = issue_list(&json!([
            {"title": "  First  ", "body": "Details", "labels": ["dogfood", " ui "]},
            {"title": "Second"},
        ]))
        .unwrap();
        assert_eq!(
            issues,
            vec![
                json!({"title": "First", "body": "Details", "labels": ["dogfood", "ui"]}),
                json!({"title": "Second", "body": "", "labels": []}),
            ]
        );
        assert!(issue_list(&json!([])).is_err());
        assert!(issue_list(&json!([{"title": " "}])).is_err());
        assert!(issue_list(&json!([{"title": "a", "labels": "ui"}])).is_err());
        assert!(issue_list(&json!([{"title": "a", "labels": [""]}])).is_err());
        assert!(issue_list(&json!(vec![json!({"title": "a"}); MAX_ISSUES + 1])).is_err());
    }

    #[test]
    fn edits_issues() {
        let payload = json!({"repo": "a/one", "issues": [{"title": "One", "body": "", "labels": []}, {"title": "Two", "body": "", "labels": []}]});
        assert_eq!(edit_issues(&payload, &json!({})).unwrap(), payload);
        let edited = edit_issues(&payload, &json!({"issues": [{"title": "Two, edited"}]})).unwrap();
        assert_eq!(edited["repo"], "a/one");
        assert_eq!(
            edited["issues"],
            json!([{"title": "Two, edited", "body": "", "labels": []}])
        );
        assert!(edit_issues(&payload, &json!({"issues": []})).is_err());
    }

    #[test]
    fn edits_pull_requests() {
        let payload = json!({"repo": "a/one", "title": "Old", "body": "Old body"});
        let edited = edit_pull_request(&payload, &json!({"title": "New"})).unwrap();
        assert_eq!(edited["title"], "New");
        assert_eq!(edited["body"], "Old body");
        assert!(edit_pull_request(&payload, &json!({"title": " "})).is_err());
        assert!(edit_pull_request(&payload, &json!({"body": 1})).is_err());
        assert_eq!(
            edit_pull_request(&payload, &json!({"auto_merge": true})).unwrap()["auto_merge"],
            true
        );
        assert!(edit_pull_request(&payload, &json!({"auto_merge": "yes"})).is_err());
    }

    #[test]
    fn reviews_a_stack_as_a_whole() {
        let payload = json!({"repo": "a/one", "base": "main", "pull_requests": [
            {"head": "s-1", "base": "main", "head_sha": "111", "title": "One", "body": "First."},
            {"head": "s-2", "base": "s-1", "head_sha": "222", "title": "Two", "body": "Second."},
        ]});
        let subject = stack_review_subject(&payload).unwrap();
        assert_eq!(
            (
                subject.base.as_str(),
                subject.head.as_str(),
                subject.head_sha.as_str()
            ),
            ("main", "s-2", "222")
        );
        assert_eq!(subject.title, "a stack of 2 pull requests");
        assert!(
            subject
                .body
                .contains("## 2. Two (`s-2` into `s-1`)\n\nSecond.")
        );
    }

    #[test]
    fn edits_stacks_by_position() {
        let payload = json!({"repo": "a/one", "pull_requests": [
            {"head": "s-1", "title": "One", "body": "1"},
            {"head": "s-2", "title": "Two", "body": "2"},
        ]});
        let edited = edit_stack(
            &payload,
            &json!({"pull_requests": [{}, {"title": "Second"}]}),
        )
        .unwrap();
        assert_eq!(edited["pull_requests"][0]["title"], "One");
        assert_eq!(edited["pull_requests"][1]["title"], "Second");
        assert_eq!(edited["pull_requests"][1]["body"], "2");
        assert_eq!(edit_stack(&payload, &json!({})).unwrap(), payload);
        assert!(edit_stack(&payload, &json!({"pull_requests": [{}, {}, {}]})).is_err());
        assert!(edit_stack(&payload, &json!({"pull_requests": [{"title": ""}]})).is_err());
        assert_eq!(
            edit_stack(&payload, &json!({"auto_merge": true})).unwrap()["auto_merge"],
            true
        );
        assert!(edit_stack(&payload, &json!({"auto_merge": "yes"})).is_err());
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
