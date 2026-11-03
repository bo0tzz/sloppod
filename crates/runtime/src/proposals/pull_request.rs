//! Opening pull requests from the session's branches: one, or a stack of them (ADR 3).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Followed, Manifest, ProposalKind, RepoGrant, Result, ReviewSubject, SessionContext,
    ToolSpec,
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
            whole: false,
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
        let base = super::current_base(&app, session, repo, base).await?;
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

    async fn follow(&self, session: &SessionContext, payload: &Value) -> Result<Option<Followed>> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let (Some(base), Some(head), Some(head_sha)) = (
            payload["base"].as_str(),
            payload["head"].as_str(),
            payload["head_sha"].as_str(),
        ) else {
            return Ok(None);
        };
        let app = self.app(session, repo).await?;
        let Some(followed) = follow_base(&app, repo, base, head, head_sha).await? else {
            return Ok(None);
        };
        let mut arguments = payload.clone();
        arguments["base"] = json!(followed.base);
        let payload = self.prepare(session, arguments).await?;
        // A push since the comparison is a change nobody has looked at.
        let same_change = followed.same_change && payload["head_sha"] == followed.head_sha.as_str();
        Ok(Some(Followed {
            payload,
            note: followed.note,
            same_change,
        }))
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
        whole: false,
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
        base = super::current_base(&app, session, repo, base).await?;
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

    /// Only the bottom pull request's base can merge before the stack is opened; the layers above stay on theirs.
    async fn follow(&self, session: &SessionContext, payload: &Value) -> Result<Option<Followed>> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let layers = stack_layers(payload);
        let (Some(base), Some(bottom)) = (payload["base"].as_str(), layers.first()) else {
            return Ok(None);
        };
        let app = self.app(session, repo).await?;
        let Some(followed) = follow_base(
            &app,
            repo,
            base,
            bottom["head"].as_str().unwrap_or_default(),
            bottom["head_sha"].as_str().unwrap_or_default(),
        )
        .await?
        else {
            return Ok(None);
        };
        let mut arguments = payload.clone();
        arguments["base"] = json!(followed.base);
        let prepared = self.prepare(session, arguments).await?;
        let same_change = followed.same_change && pins_held(payload, &prepared, &followed.head_sha);
        Ok(Some(Followed {
            payload: prepared,
            note: followed.note,
            same_change,
        }))
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

/// Whether a stack prepared again after its base moved pins what was compared: its bottom at `bottom`, the commit its
/// change was fingerprinted at, and every layer above where it was proposed. A push to any is unreviewed.
fn pins_held(proposed: &Value, prepared: &Value, bottom: &str) -> bool {
    let pins = |payload| {
        stack_layers(payload)
            .iter()
            .map(|l| l["head_sha"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
    };
    let mut expected = pins(proposed);
    if let Some(first) = expected.first_mut() {
        *first = bottom.to_owned();
    }
    pins(prepared) == expected
}

/// A base branch that merged and is gone, followed to where it went.
#[derive(Debug, PartialEq)]
struct MovedBase {
    /// The branch it merged into: still there, unless that merged too, as a stack merging from the bottom up does.
    base: String,
    /// The old base's last commit, as it merged.
    merged_sha: String,
    /// The pull requests it took, the old base's first.
    pulls: Vec<u64>,
}

/// How many merged branches deep a base is followed.
const FOLLOW_DEPTH: usize = 8;

/// Where `base` went: `None` while it's there, or when it's gone without being merged, which leaves nothing to follow.
async fn moved_base(app: &GitHubApp, repo: &str, base: &str) -> Result<Option<MovedBase>> {
    if app.branch_exists(repo, base).await.map_err(err)? {
        return Ok(None);
    }
    let mut branch = base.to_owned();
    let mut merged_sha = None;
    let mut pulls = Vec::new();
    for _ in 0..FOLLOW_DEPTH {
        let Some(pull) = app.merged_pull_from(repo, &branch).await.map_err(err)? else {
            return Ok(None);
        };
        let (Some(into), Some(sha), Some(number)) = (
            pull["base"]["ref"].as_str(),
            pull["head"]["sha"].as_str(),
            pull["number"].as_u64(),
        ) else {
            return Ok(None);
        };
        merged_sha.get_or_insert_with(|| sha.to_owned());
        pulls.push(number);
        branch = into.to_owned();
        if app.branch_exists(repo, &branch).await.map_err(err)? {
            return Ok(merged_sha.map(|merged_sha| MovedBase {
                base: branch,
                merged_sha,
                pulls,
            }));
        }
    }
    Ok(None)
}

/// A pull request from `head`, proposed at `head_sha` onto `base`, moved off a base that merged and is gone onto
/// where it went, as GitHub retargets an open one (#141).
struct FollowedBase {
    base: String,
    note: String,
    /// Whether its diff at `head_sha` is still the one proposed, by `rebases::fingerprint`.
    same_change: bool,
    /// The commit of `head` that was compared: what preparing it again has to pin for `same_change` to hold.
    head_sha: String,
}

/// Follows a merged base. With a merge commit, what merged with it is in the new base's history and drops out of the
/// comparison by itself; squashed or rebased, it isn't, so the new base is merged into `head` to leave it out, as
/// `git merge` would. A conflict leaves that to the author.
async fn follow_base(
    app: &GitHubApp,
    repo: &str,
    base: &str,
    head: &str,
    head_sha: &str,
) -> Result<Option<FollowedBase>> {
    let Some(moved) = moved_base(app, repo, base).await? else {
        return Ok(None);
    };
    let new = moved.base.as_str();
    let pulls = moved
        .pulls
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut note = format!(
        "`{base}` merged into `{new}` ({pulls}) and is gone, so it now merges into `{new}`"
    );
    let mut tip = app.branch_sha(repo, head).await.map_err(err)?;
    if !app
        .contains(repo, new, &moved.merged_sha)
        .await
        .map_err(err)?
    {
        let message = format!(
            "Merge {new} into {head}\n\n{base} was squashed or rebased into {new}; this leaves out its commits."
        );
        match app.merge_into(repo, head, new, &message).await.map_err(err)? {
            Some(merged) => {
                note.push_str(&format!(
                    ". It was squashed or rebased, so sloppod merged `{new}` into `{head}` (now at {merged:.7}) to \
                     leave out what merged with it; pull before you push to `{head}` again"
                ));
                tip = merged;
            }
            None => note.push_str(&format!(
                ". It was squashed or rebased, and `{new}` conflicts with `{head}`, so the change still carries what \
                 merged with `{base}`: merge `{new}` into `{head}` yourself"
            )),
        }
    }
    let proposed = app
        .diff_to(repo, &moved.merged_sha, head_sha)
        .await
        .map_err(err)?;
    let now = app.diff_to(repo, new, &tip).await.map_err(err)?;
    let same_change = {
        let proposed = crate::rebases::fingerprint(&proposed);
        proposed.is_some() && proposed == crate::rebases::fingerprint(&now)
    };
    Ok(Some(FollowedBase {
        base: new.to_owned(),
        note,
        same_change,
        head_sha: tip,
    }))
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
    fn a_followed_stack_keeps_its_review_only_at_the_commits_compared() {
        let proposed = json!({"pull_requests": [{"head_sha": "b1"}, {"head_sha": "c1"}]});
        // The bottom merged its new base in, at m1, and nothing else moved.
        let prepared = json!({"pull_requests": [{"head_sha": "m1"}, {"head_sha": "c1"}]});
        assert!(pins_held(&proposed, &prepared, "m1"));
        // A push to the bottom after it was compared, or to a layer above.
        assert!(!pins_held(&proposed, &prepared, "b1"));
        let pushed = json!({"pull_requests": [{"head_sha": "m1"}, {"head_sha": "c2"}]});
        assert!(!pins_held(&proposed, &pushed, "m1"));
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

    /// A fake GitHub where `sloppod/a-one` merged into `main` as #7, at `a1`, and is gone, and `sloppod/a-two`, at
    /// `b1`, builds on it: proposed with `sloppod/a-one` as its base.
    mod merged_base {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};

        use axum::extract::{Path, Query, State};
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::{IntoResponse, Response};
        use axum::routing::{get, post};
        use axum::{Json, Router};

        use super::*;

        const ONE: &str = "diff --git a/one b/one\n+one\n";
        const TWO: &str = "diff --git a/two b/two\n+two\n";

        #[derive(Default)]
        struct Fake {
            branches: Mutex<HashMap<String, String>>,
            merged: Vec<Value>,
            /// Commits in `main`'s history.
            in_main: Vec<&'static str>,
            /// Diffs by `base...sha`.
            diffs: HashMap<String, String>,
            conflicts: bool,
            writes: Mutex<Vec<String>>,
        }

        type Shared = State<Arc<Fake>>;

        async fn branch(
            State(fake): Shared,
            Path((_, _, name)): Path<(String, String, String)>,
        ) -> Response {
            match fake.branches.lock().unwrap().get(&name) {
                Some(sha) => Json(json!({"name": name, "commit": {"sha": sha}})).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }

        async fn commit(
            State(fake): Shared,
            Path((_, _, name)): Path<(String, String, String)>,
        ) -> Json<Value> {
            let sha = fake
                .branches
                .lock()
                .unwrap()
                .get(&name)
                .cloned()
                .unwrap_or(name);
            Json(json!({"sha": sha}))
        }

        async fn pulls(
            State(fake): Shared,
            Query(query): Query<HashMap<String, String>>,
        ) -> Json<Value> {
            let head = query["head"].split_once(':').unwrap().1.to_owned();
            assert_eq!(query["state"], "closed");
            Json(json!(
                fake.merged
                    .iter()
                    .filter(|p| p["head"]["ref"] == head.as_str())
                    .collect::<Vec<_>>()
            ))
        }

        async fn compare(
            State(fake): Shared,
            Path((_, _, range)): Path<(String, String, String)>,
            headers: HeaderMap,
        ) -> Response {
            if headers["accept"] == "application/vnd.github.diff" {
                let tip = |name: &str| {
                    fake.branches
                        .lock()
                        .unwrap()
                        .get(name)
                        .cloned()
                        .unwrap_or(name.into())
                };
                let (base, sha) = range.split_once("...").unwrap();
                // Named by branch or by commit alike.
                let key = format!("{base}...{}", tip(sha));
                return fake
                    .diffs
                    .get(&key)
                    .cloned()
                    .unwrap_or_default()
                    .into_response();
            }
            let (sha, reference) = range.split_once("...").unwrap();
            assert_eq!(reference, "main");
            let status = if fake.in_main.contains(&sha) {
                "ahead"
            } else {
                "diverged"
            };
            Json(json!({"status": status})).into_response()
        }

        async fn merge(State(fake): Shared, Json(body): Json<Value>) -> Response {
            let (into, from) = (
                body["base"].as_str().unwrap(),
                body["head"].as_str().unwrap(),
            );
            fake.writes
                .lock()
                .unwrap()
                .push(format!("merge {from} into {into}"));
            if fake.conflicts {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"message": "Merge conflict"})),
                )
                    .into_response();
            }
            fake.branches
                .lock()
                .unwrap()
                .insert(into.to_owned(), "m1".into());
            (StatusCode::CREATED, Json(json!({"sha": "m1"}))).into_response()
        }

        fn fake(squashed: bool) -> Fake {
            let mut fake = Fake {
                merged: vec![json!({
                    "number": 7, "merged_at": "2026-11-03T08:00:00Z",
                    "head": {"ref": "sloppod/a-one", "sha": "a1"}, "base": {"ref": "main"},
                })],
                in_main: if squashed { vec![] } else { vec!["a1"] },
                ..Fake::default()
            };
            fake.branches.lock().unwrap().extend([
                ("main".to_owned(), "c1".to_owned()),
                ("sloppod/a-two".to_owned(), "b1".to_owned()),
            ]);
            // The change as proposed, and as it compares with `main` afterwards: a squash leaves `sloppod/a-one`'s
            // change in it until `main` is merged in.
            fake.diffs.insert("a1...b1".into(), TWO.into());
            let after = if squashed {
                format!("{ONE}{TWO}")
            } else {
                TWO.into()
            };
            fake.diffs.insert("main...b1".into(), after);
            fake.diffs.insert("main...m1".into(), TWO.into());
            fake
        }

        async fn github(fake: Fake) -> (Arc<Fake>, GitHubApp) {
            let fake = Arc::new(fake);
            let router = Router::new()
                .route("/repos/{owner}/{name}/branches/{*branch}", get(branch))
                .route("/repos/{owner}/{name}/commits/{*reference}", get(commit))
                .route("/repos/{owner}/{name}/pulls", get(pulls))
                .route("/repos/{owner}/{name}/compare/{*range}", get(compare))
                .route("/repos/{owner}/{name}/merges", post(merge))
                .with_state(fake.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let api = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, router).await });
            (
                fake,
                GitHubApp::for_user_with_api(&api, "token".into(), "octo".into()).unwrap(),
            )
        }

        async fn follow(app: &GitHubApp) -> Option<FollowedBase> {
            follow_base(app, "o/r", "sloppod/a-one", "sloppod/a-two", "b1")
                .await
                .unwrap()
        }

        #[tokio::test]
        async fn a_merge_commit_leaves_the_branch_as_it_is() {
            let (fake, app) = github(fake(false)).await;
            let followed = follow(&app).await.unwrap();
            assert_eq!(followed.base, "main");
            assert!(followed.same_change);
            assert_eq!(followed.head_sha, "b1");
            assert!(
                followed
                    .note
                    .contains("`sloppod/a-one` merged into `main` (#7)"),
                "{}",
                followed.note
            );
            assert!(fake.writes.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn a_squash_is_left_out_by_merging_the_new_base_in() {
            let (fake, app) = github(fake(true)).await;
            let followed = follow(&app).await.unwrap();
            assert_eq!(followed.base, "main");
            // What's left is the change that was proposed.
            assert!(followed.same_change);
            assert_eq!(followed.head_sha, "m1");
            assert!(
                followed
                    .note
                    .contains("merged `main` into `sloppod/a-two` (now at m1)"),
                "{}",
                followed.note
            );
            assert_eq!(
                *fake.writes.lock().unwrap(),
                ["merge main into sloppod/a-two"]
            );
        }

        #[tokio::test]
        async fn a_conflict_moves_it_and_leaves_the_merge_to_the_author() {
            let (_, app) = github(Fake {
                conflicts: true,
                ..fake(true)
            })
            .await;
            let followed = follow(&app).await.unwrap();
            assert_eq!(followed.base, "main");
            // It carries `sloppod/a-one`'s change too now, which wasn't reviewed with it.
            assert!(!followed.same_change);
            assert!(
                followed
                    .note
                    .contains("merge `main` into `sloppod/a-two` yourself"),
                "{}",
                followed.note
            );
        }

        #[tokio::test]
        async fn a_stack_that_merged_from_the_bottom_up_is_followed_to_the_end() {
            let mut below = fake(false);
            // `sloppod/a-one` merged into `sloppod/a-zero`, which then merged into `main`; both are gone.
            below.merged = vec![
                json!({"number": 7, "merged_at": "2026-11-03T08:00:00Z",
                       "head": {"ref": "sloppod/a-one", "sha": "a1"}, "base": {"ref": "sloppod/a-zero"}}),
                json!({"number": 6, "merged_at": "2026-11-03T08:01:00Z",
                       "head": {"ref": "sloppod/a-zero", "sha": "a1"}, "base": {"ref": "main"}}),
            ];
            let (_, app) = github(below).await;
            let followed = follow(&app).await.unwrap();
            assert_eq!(followed.base, "main");
            assert!(followed.note.contains("(#7, #6)"), "{}", followed.note);
        }

        #[tokio::test]
        async fn a_base_that_is_there_or_went_unmerged_is_left_alone() {
            let there = fake(false);
            there
                .branches
                .lock()
                .unwrap()
                .insert("sloppod/a-one".into(), "a1".into());
            let (seen, app) = github(there).await;
            assert!(follow(&app).await.is_none());
            assert!(seen.writes.lock().unwrap().is_empty());

            let (_, app) = github(Fake {
                merged: vec![],
                ..fake(false)
            })
            .await;
            assert!(follow(&app).await.is_none());
        }
    }
}
