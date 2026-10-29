//! Opening pull requests from the session's branches: one, or a stack of them (ADR 3).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Manifest, ProposalKind, RepoGrant, Result, ReviewSubject, SessionContext, ToolSpec,
};

use super::{branch_detail, err, head_branch, target_repo};
use crate::credentials::Credentials;
use crate::github::{GitHubApp, PushLog, StackMerge};

pub struct PullRequest {
    credentials: Arc<Credentials>,
}

impl PullRequest {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }

    /// What reads the comparison and opens the PR: the identity the session acts with on `repo` (ADR 29).
    async fn app(&self, session: &SessionContext, repo: &str) -> Result<Arc<GitHubApp>> {
        super::github_for(&self.credentials, session, repo).await
    }
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
                          commits first, and don't push more to the branch before it's approved: new commits \
                          make the approved proposal fail to open, and you'd propose it again. A human reviews \
                          the title, body and diff; once approved, sloppod opens the PR. Returns the proposal's \
                          id; check it with get_proposal."
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
                    "from_sub_session": {"type": "string", "description": "The id of the sub-session whose passed report this work comes from: its reviewer, who knows the work, reviews what changed since it passed instead of the whole change. Needs review"},
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
            describes_change: true,
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
        let app = self.app(session, repo).await?;
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
        let repo = payload["repo"].as_str().unwrap_or_default();
        branch_detail(&*self.app(session, repo).await?, payload).await
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        edit_pull_request(payload, edits)
    }

    async fn execute(
        &self,
        session: &SessionContext,
        _proposal: uuid::Uuid,
        payload: &Value,
    ) -> Result<Value> {
        let request = json!({
            "title": payload["title"],
            "body": payload["body"],
            "head": payload["head"],
            "base": payload["base"],
            "draft": payload["draft"],
        });
        let repo = payload["repo"].as_str().unwrap_or_default();
        let head = payload["head"].as_str().unwrap_or_default();
        let app = self.app(session, repo).await?;
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
                // What a later push is compared with, to keep auto-merge on through a rebase (#106).
                let base = payload["base"].as_str().unwrap_or_default();
                match app.diff_to(repo, base, reviewed).await {
                    Ok(diff) => {
                        if let Some(fingerprint) = crate::rebases::fingerprint(&diff) {
                            result["fingerprint"] = json!(fingerprint);
                        }
                    }
                    Err(e) => tracing::warn!(%repo, error = %e, "fingerprinting a pull request"),
                }
                format!("{url} is open and merges once its required checks pass.")
            }
            Err(e) => {
                result["auto_merge"] = json!(false);
                format!(
                    "{url} is open; auto-merge couldn't be turned on ({e}), so it needs merging by hand. {}",
                    crate::pulls::TELL_THE_USER
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
    pushes: Arc<PushLog>,
}

impl Stack {
    pub fn new(credentials: Arc<Credentials>, pushes: Arc<PushLog>) -> Self {
        Self {
            credentials,
            pushes,
        }
    }

    async fn app(&self, session: &SessionContext, repo: &str) -> Result<Arc<GitHubApp>> {
        super::github_for(&self.credentials, session, repo).await
    }
}

pub(super) fn stack_layers(payload: &Value) -> &[Value] {
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
        describes_change: true,
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
                          Push every branch first, and don't push more to any of them before it's approved: new \
                          commits make the approved proposal fail to open, and you'd propose it again. They come \
                          from your session branch or branches named after it. A human reviews them together; \
                          once approved, sloppod opens them and links them as a stack on GitHub. Returns the \
                          proposal's id; check it with get_proposal."
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
                    "from_sub_session": {"type": "string", "description": "The id of the sub-session whose passed report this work comes from: its reviewer, who knows the work, reviews what changed since it passed instead of the whole change. Needs review"},
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
        let app = self.app(session, repo).await?;
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
        let repo = payload["repo"].as_str().unwrap_or_default();
        let app = self.app(session, repo).await?;
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

    async fn execute(
        &self,
        session: &SessionContext,
        _proposal: uuid::Uuid,
        payload: &Value,
    ) -> Result<Value> {
        let started = std::time::Instant::now();
        let repo = payload["repo"].as_str().unwrap_or_default();
        let app = self.app(session, repo).await?;
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
            let pinned: Vec<(&str, &str)> = layers
                .iter()
                .map(|l| {
                    (
                        l["head"].as_str().unwrap_or_default(),
                        l["head_sha"].as_str().unwrap_or_default(),
                    )
                })
                .collect();
            message.push_str(&match (&linked, pulls.last()) {
                (Ok(stack), Some(top)) => match app
                    .auto_merge_stack(
                        repo,
                        &StackMerge {
                            stack: *stack,
                            pulls: &numbers,
                            top_node_id: top["node_id"].as_str().unwrap_or_default(),
                            layers: &pinned,
                        },
                        &self.pushes,
                        started,
                    )
                    .await
                {
                    Ok(()) => {
                        auto_merge = true;
                        " The stack merges once its required checks pass.".to_owned()
                    }
                    Err(e) => format!(
                        " Auto-merge couldn't be turned on ({e}), so it needs merging by hand. {}",
                        crate::pulls::TELL_THE_USER
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
