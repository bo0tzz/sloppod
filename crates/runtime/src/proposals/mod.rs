//! Proposals (ADR 3): the kinds an agent can propose, and proposing, deciding and carrying them out.

mod act_as;
mod code_review;
mod issue_updates;
mod issues;
mod pull_request;
mod pull_request_updates;
mod repo_access;
mod report;
mod updates;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use sloppod_contracts::{Error, ProposalKind, RepoGrant, Result, SessionContext, ToolSpec};
use sloppod_protocol::api::{Proposal, ProposalStatus, ReviewPolicy};
use tracing::Instrument;
use uuid::Uuid;

use crate::github::{GitHubApp, StackWatch};
use crate::{AppState, db, lifecycle, reviews};

pub use act_as::ActAsOwner;
pub use code_review::{CODE_REVIEW, CodeReview};
pub use issue_updates::IssueUpdates;
pub use issues::Issues;
use pull_request::stack_layers;
pub use pull_request::{PullRequest, Stack};
pub use pull_request_updates::PullRequestUpdates;
pub use repo_access::{RepoAccess, grant_repo, grant_repo_as, repo_granted, repo_granted_as};
pub use report::{REPORT, Report};

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

fn err(e: impl std::fmt::Display) -> Error {
    Error::new(e.to_string())
}

/// The branch a session pushes to; the same in every repository it can write to.
pub fn session_branch(session: Uuid) -> String {
    format!("sloppod/{}", &session.simple().to_string()[..8])
}

/// Whether a session may push `branch`: its session branch, or one named after it (`<session branch>-<name>`).
/// Not `<session branch>/<name>`, which git can't have next to the session branch itself.
/// The name may only have letters, digits, `.`, `_` and `-`, so it means the same in a URL as in git: a `+` or `#` in
/// it would change which branch the GitHub API is asked about, e.g. when turning off auto-merge after a push.
pub fn is_session_branch(session_branch: &str, branch: &str) -> bool {
    branch == session_branch
        || branch
            .strip_prefix(session_branch)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|name| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            })
}

/// The branch a pull request or report is from: the session branch unless one named after it is named.
pub(crate) fn head_branch<'a>(session_branch: &'a str, named: Option<&'a str>) -> Result<&'a str> {
    let Some(head) = named.map(str::trim).filter(|h| !h.is_empty()) else {
        return Ok(session_branch);
    };
    let head = head.strip_prefix("refs/heads/").unwrap_or(head);
    if is_session_branch(session_branch, head) {
        Ok(head)
    } else {
        Err(Error::new(format!(
            "the branch has to be your session branch `{session_branch}` or one named after it, like \
             `{session_branch}-<name>`, where the name has only letters, digits, `.`, `_` and `-`"
        )))
    }
}

/// What a reviewer of a branch sees: the diff and commits from the base to the commit that was proposed, not
/// wherever the branch is now. Nothing for a payload without one.
async fn branch_detail(app: &GitHubApp, payload: &Value) -> Result<Value> {
    let Some(head) = payload["head_sha"].as_str() else {
        return Ok(json!({}));
    };
    let comparison = app
        .compare(
            payload["repo"].as_str().unwrap_or_default(),
            payload["base"].as_str().unwrap_or_default(),
            head,
        )
        .await
        .map_err(err)?;
    Ok(json!({
        "diff": comparison.diff,
        "commits": comparison.commits.iter().map(|(sha, message)| json!({"sha": sha, "message": message})).collect::<Vec<_>>(),
    }))
}

/// The repository a pull request or issue is for: the one named, or the only one the session can push to.
/// The client a proposal reads and acts with on `repo` (ADR 29): the identity the session's grant of `repo` names, or
/// the GitHub App. Every proposal kind gets its client here.
pub(crate) async fn github_for(
    credentials: &crate::credentials::Credentials,
    session: &SessionContext,
    repo: &str,
) -> Result<Arc<GitHubApp>> {
    let credential = session.repo_credential(repo);
    crate::github::record_credential(credential);
    credentials
        .github_as(&session.owner, credential)
        .await
        .map_err(|e| Error::new(e.to_string()))
}

/// A hidden mark for the `item`th thing a proposal puts on GitHub (an issue, a comment). Carrying the proposal out
/// again (a restart before its outcome was recorded) looks for this, rather than for something that only says the
/// same, which could be anyone's (#346).
pub(crate) fn marker(proposal: Uuid, item: usize) -> String {
    format!("<!-- sloppod:proposal/{proposal}/{item} -->")
}

/// `text` with `marker` under it, out of sight in rendered Markdown.
pub(crate) fn marked(text: &str, marker: &str) -> String {
    if text.trim().is_empty() {
        marker.to_owned()
    } else {
        format!("{text}\n\n{marker}")
    }
}

/// From when to look for what a proposal put on GitHub: an hour before it was made, for clocks that disagree.
/// Proposal ids are UUIDv7, so they say when; any other id looks from the start.
pub(crate) fn made_since(proposal: Uuid) -> chrono::DateTime<chrono::Utc> {
    proposal
        .get_timestamp()
        .and_then(|at| {
            let (secs, _) = at.to_unix();
            chrono::DateTime::from_timestamp(i64::try_from(secs).ok()?, 0)
        })
        .map(|at| at - chrono::Duration::hours(1))
        .unwrap_or_default()
}

/// Prepares a proposal's payload from the agent's arguments, for its first round and every later one: the kind's own
/// preparation, under the session's `acting_span`, plus `as`, who it acts as (ADR 29), which is shown wherever it's
/// reviewed and held to when it's carried out.
pub(crate) async fn prepared(
    kind: &dyn ProposalKind,
    session: &SessionContext,
    arguments: Value,
) -> Result<Value> {
    let span = crate::github::acting_span(&session.session_id, None);
    let mut payload = kind.prepare(session, arguments).instrument(span).await?;
    if let Some(login) = kind
        .target(&payload)
        .and_then(|repo| session.repo_acting_as(&repo).map(str::to_owned))
    {
        payload["as"] = json!(login);
    }
    Ok(payload)
}

/// A proposal is carried out as whoever it said it would act as when it was approved (`as` in its payload, or the
/// App): if the session's grant of its repository changed identity since, nothing is done.
fn same_identity(session: &SessionContext, target: Option<&str>, payload: &Value) -> Result<()> {
    let now = target.and_then(|repo| session.repo_acting_as(repo));
    let proposed = payload["as"].as_str();
    if now == proposed {
        return Ok(());
    }
    let who = |login: Option<&str>| login.map_or("the GitHub App".to_owned(), str::to_owned);
    Err(Error::new(format!(
        "it was proposed to act as {}, but the session now acts as {} there, so nothing was done; propose it again",
        who(proposed),
        who(now)
    )))
}

/// How a refusal for a repository the session can't push to ends: what to do about it, if anything.
fn next_step(session: &SessionContext) -> String {
    session.role.repo_next_step()
}

fn target_repo<'a>(session: &'a SessionContext, named: Option<&str>) -> Result<RepoGrant<'a>> {
    if let Some(repo) = named {
        return session.writable_repo_for(repo).ok_or_else(|| {
            Error::new(format!(
                "this session can't push to {repo}{}",
                next_step(session)
            ))
        });
    }
    let writable: Vec<RepoGrant> = session.writable_repos().collect();
    match writable.as_slice() {
        [] => Err(Error::new(format!(
            "this session can't push to any repository{}",
            next_step(session)
        ))),
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
    let asked = arguments["review"].as_bool() == Some(true) || kind.is_review();
    let required = session.review == ReviewPolicy::Required;
    // A sub-session's passed report whose review this one carries over (#247).
    let from_sub_session = arguments["from_sub_session"].as_str().map(str::to_owned);
    let context = session.context();
    let payload = prepared(kind.as_ref(), &context, arguments)
        .await
        .map_err(|e| e.0)?;
    let mut subject = kind.review_subject(&payload).filter(|_| asked || required);
    if asked && subject.is_none() {
        return Err("proposals of this kind aren't reviewed; propose it without review".into());
    }
    if subject.is_some()
        && !reviews::available(state, &session.owner)
            .await
            .map_err(|e| e.to_string())?
    {
        if kind.is_review() {
            return Err(
                "reviews need a ChatGPT login in sloppod's settings, which the user hasn't added"
                    .into(),
            );
        }
        if asked {
            return Err(
                "reviews need a ChatGPT login in sloppod's settings, which the user hasn't added; \
                        propose it without review"
                    .into(),
            );
        }
        subject = None;
    }
    let carried = match (&from_sub_session, &subject) {
        (Some(child), Some(subject)) => {
            reviews::carried_over(state, &session, child, subject).await?
        }
        (Some(_), None) => {
            return Err(
                "`from_sub_session` carries a review over, so this proposal needs one: add `review: true`".into(),
            );
        }
        (None, _) => None,
    };
    let status = if subject.is_some() {
        ProposalStatus::InReview
    } else {
        ProposalStatus::Pending
    };
    let row = db::insert_proposal(&state.db, session_id, &kind.kind(), status, &payload)
        .await
        .map_err(|e| e.to_string())?;
    let proposal = row.to_api();
    state.hub.broadcast_proposal(&proposal);
    let Some(subject) = subject else {
        return settle(state, &session, kind.as_ref(), proposal, None)
            .await
            .map_err(|e| e.to_string());
    };
    if let Err(e) = reviews::start(state, &row, &subject, None, carried.as_ref()).await {
        tracing::warn!(proposal = %row.id, error = %e, "starting a review");
        let row = db::move_proposal(
            &state.db,
            row.id,
            ProposalStatus::InReview,
            ProposalStatus::Pending,
            None,
        )
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the proposal changed meanwhile")?;
        state.hub.broadcast_proposal(&row.to_api());
        if !kind.outward() {
            settle(
                state,
                &session,
                kind.as_ref(),
                row.to_api(),
                Some(json!({"verdict": "stalled"})),
            )
            .await
            .map_err(|e| e.to_string())?;
            if kind.is_review() {
                return Err(format!("the review couldn't start ({e}); try again later"));
            }
            return Err(format!(
                "the review couldn't start ({e}), so proposal {} is being carried out without one",
                row.id
            ));
        }
        return Err(format!(
            "the review couldn't start ({e}), so proposal {} went to the user without one",
            row.id
        ));
    }
    Ok(proposal)
}

/// What happens to a proposal once it's out of review, or wasn't reviewed: `review` is how its review ended,
/// `{verdict: "pass" | "changes" | "stalled", summary?, findings?}`. One that isn't outward is carried out, with the
/// review's outcome added. Otherwise a standing approval may approve it, but only one that passed review or wasn't
/// reviewed (ADR 24); the rest wait for the user.
pub async fn settle(
    state: &AppState,
    session: &db::SessionRow,
    kind: &dyn ProposalKind,
    proposal: Proposal,
    review: Option<Value>,
) -> anyhow::Result<Proposal> {
    if !kind.outward() {
        let edits = review.map_or_else(|| json!({}), |review| json!({"review": review}));
        return match decide(
            state,
            proposal.id,
            Verdict::Approve { edits },
            &session.owner,
            "sloppod",
        )
        .await
        {
            Ok(Some(approved)) => Ok(approved),
            Ok(None) | Err(DecideError::Decided) => Ok(proposal),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        };
    }
    if review.is_some_and(|review| review["verdict"] != "pass") {
        return Ok(proposal);
    }
    apply_standing_approval(state, session, kind, proposal).await
}

/// Approves a proposal that's waiting for a human when one of the owner's standing approvals covers it.
async fn apply_standing_approval(
    state: &AppState,
    session: &db::SessionRow,
    kind: &dyn ProposalKind,
    proposal: Proposal,
) -> anyhow::Result<Proposal> {
    if !kind.standing_approvable() {
        return Ok(proposal);
    }
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

/// Why a decision wasn't recorded.
#[derive(Debug, thiserror::Error)]
pub enum DecideError {
    /// The edits don't apply to the proposal.
    #[error("{0}")]
    Invalid(String),
    #[error("the proposal was already decided")]
    Decided,
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

impl From<sqlx::Error> for DecideError {
    fn from(e: sqlx::Error) -> Self {
        DecideError::Failed(e.into())
    }
}

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
) -> std::result::Result<Option<Proposal>, DecideError> {
    let Some(row) = owned_proposal(state, id, owner).await? else {
        return Ok(None);
    };
    let kind = state
        .proposals
        .get(&row.kind)
        .ok_or_else(|| anyhow::anyhow!("no executor for {}", row.kind))?;
    let (status, payload, reason) = match verdict {
        Verdict::Approve { edits } => (
            ProposalStatus::Approved,
            kind.edit(&row.payload, &edits)
                .map_err(|e| DecideError::Invalid(e.0))?,
            None,
        ),
        Verdict::Reject { reason } => (ProposalStatus::Rejected, row.payload.clone(), reason),
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
        return Err(DecideError::Decided);
    };
    state.hub.broadcast_proposal(&row.to_api());
    let summary = kind.summary(&payload);
    if status == ProposalStatus::Rejected {
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

/// The longest wait between tries to record a proposal's outcome.
const OUTCOME_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// Records a proposal's outcome with `write`, trying again with backoff until it lands. The work it records is done
/// by then, so a failed write (the database briefly away) must neither drop its result nor leave the proposal
/// approved for the next startup to carry out again (#326).
async fn until_recorded<F, Fut, R>(id: Uuid, mut write: F) -> R
where
    F: FnMut() -> Fut,
    Fut: Future<Output = sqlx::Result<R>>,
{
    let mut wait = std::time::Duration::from_secs(1);
    loop {
        match write().await {
            Ok(written) => return written,
            Err(e) => {
                tracing::warn!(proposal = %id, error = %e, "recording a proposal's outcome; trying again");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(OUTCOME_RETRY_MAX);
            }
        }
    }
}

/// Carries a proposal out once with `execute`, then records what it came to (its result, or why it failed) with
/// `record`, trying the write again until it lands. The work is never repeated for a write that failed.
async fn execute_then_record<F, Fut, R>(
    id: Uuid,
    execute: impl Future<Output = Result<Value>>,
    mut record: F,
) -> R
where
    F: FnMut(std::result::Result<Value, String>) -> Fut,
    Fut: Future<Output = sqlx::Result<R>>,
{
    let executed = execute.await.map_err(|e| e.0);
    until_recorded(id, || record(executed.clone())).await
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
    let context = session.context();
    let target = kind.target(payload);
    let span = crate::github::acting_span(
        &session_id.to_string(),
        target
            .as_deref()
            .and_then(|repo| context.repo_credential(repo)),
    );
    let execute = async {
        match same_identity(&context, target.as_deref(), payload) {
            Ok(()) => kind.execute(&context, id, payload).instrument(span).await,
            Err(e) => Err(e),
        }
    };
    let finished = execute_then_record(id, execute, |executed| {
        let db = state.db.clone();
        async move {
            match executed {
                Ok(result) => {
                    db::finish_proposal(&db, id, ProposalStatus::Executed, Some(&result), None)
                        .await
                }
                Err(e) => {
                    db::finish_proposal(&db, id, ProposalStatus::Failed, None, Some(&e)).await
                }
            }
        }
    })
    .await;
    let proposal = finished.to_api();
    state.hub.broadcast_proposal(&proposal);
    // What it has to tell other sessions, e.g. a report for a sub-session's parent, which is woken for it if it
    // parked meanwhile. Not held to the tree's limit (ADR 27): the reporting sub-session is done, so its place is
    // about to free up, and a report left unread would leave its work with nobody.
    if let Some(result) = &proposal.result {
        for notice in result["notify"].as_array().into_iter().flatten() {
            if let (Some(to), Some(text)) = (
                notice["session"].as_str().and_then(|s| s.parse().ok()),
                notice["text"].as_str(),
            ) {
                let summary = notice["summary"].as_str().map(str::to_owned);
                state
                    .hub
                    .notify_summarised(to, text.to_owned(), summary)
                    .await;
                if let Ok(Some(recipient)) = db::get_session(&state.db, to).await
                    && let Err(e) = lifecycle::wake(state, &recipient).await
                {
                    tracing::warn!(session = %to, error = %e, "waking a session for a notice");
                }
            }
        }
    }
    state.hub.broadcast_session(session_id).await;
    // The session heard how its review ended already, and there was nothing else to do.
    if kind.is_review() && proposal.error.is_none() {
        return Ok(());
    }
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
    watch_stack(state, &finished);
    Ok(())
}

/// How often a stack that's set to merge itself is checked.
const STACK_WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Watches a stack that's set to merge itself until it has: if a layer moves from the commit that was proposed,
/// however it got there, its auto-merge goes off and the session hears why. Polling narrows the time a moved layer
/// could merge to a check's interval; it doesn't rule it out. Other proposals are left alone.
pub fn watch_stack(state: &AppState, proposal: &db::ProposalRow) {
    let result = proposal.result.clone().unwrap_or_default();
    if proposal.kind != "github.stack.create" || result["auto_merge"] != true {
        return;
    }
    let pulls = result["pull_requests"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let layers: Vec<(u64, String)> = pulls
        .iter()
        .zip(stack_layers(&proposal.payload))
        .filter_map(|(pull, layer)| {
            Some((
                pull["number"].as_u64()?,
                layer["head_sha"].as_str()?.to_owned(),
            ))
        })
        .collect();
    let top_node_id = pulls
        .last()
        .and_then(|pull| pull["node_id"].as_str())
        .unwrap_or_default()
        .to_owned();
    let repo = proposal.payload["repo"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let (state, id, session_id) = (state.clone(), proposal.id, proposal.session_id);
    tokio::spawn(async move {
        let layers: Vec<(u64, &str)> = layers.iter().map(|(n, sha)| (*n, sha.as_str())).collect();
        loop {
            tokio::time::sleep(STACK_WATCH_EVERY).await;
            let mut credential = None;
            let app = match db::get_session(&state.db, session_id).await {
                Ok(Some(session)) => {
                    let context = session.context();
                    credential = context.repo_credential(&repo).map(str::to_owned);
                    state
                        .credentials
                        .github_as(&context.owner, credential.as_deref())
                        .await
                }
                Ok(None) => return,
                Err(e) => Err(e.into()),
            };
            let checked = match app {
                Ok(app) => {
                    let span = crate::github::acting_span(&session_id.to_string(), None);
                    span.record("credential", credential.as_deref().unwrap_or("app"));
                    app.check_merging_stack(&repo, &top_node_id, &layers)
                        .instrument(span)
                        .await
                }
                Err(e) => Err(e),
            };
            match checked {
                Ok(StackWatch::Done) => return,
                Ok(StackWatch::Holding) => {}
                Ok(StackWatch::Moved(number)) => {
                    state
                        .hub
                        .notify(
                            session_id,
                            format!(
                                "sloppod: {} in your stack has new commits since it was approved, so the stack's \
                                 auto-merge is now off and it needs merging by hand. {} It's at {}.",
                                crate::pulls::pull_ref(&repo, number),
                                crate::pulls::TELL_THE_USER,
                                crate::pulls::pull_url(&repo, number)
                            ),
                        )
                        .await;
                    return;
                }
                Err(e) => {
                    tracing::warn!(proposal = %id, error = %e, "checking a stack that's set to merge")
                }
            }
        }
    });
}

/// Watches the stacks still set to merge themselves, e.g. after the runtime restarted.
pub async fn resume_stack_watches(state: &AppState) {
    match db::merging_stacks(&state.db).await {
        Ok(rows) => rows.iter().for_each(|row| watch_stack(state, row)),
        Err(e) => tracing::error!(error = %e, "listing stacks set to merge"),
    }
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
    let span = crate::github::acting_span(&row.session_id.to_string(), None);
    match kind
        .detail(&session.context(), &row.payload)
        .instrument(span)
        .await
    {
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
    use sloppod_contracts::Grant;

    fn session(repos: &[&str]) -> SessionContext {
        SessionContext {
            session_id: Uuid::nil().to_string(),
            owner: "dev".into(),
            role: Default::default(),
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
    fn a_proposal_is_carried_out_as_whoever_it_said_it_would_act_as() {
        let app = session(&["a/one"]);
        let mut mine = session(&["a/one"]);
        mine.grants[0].params["credential"] = json!("c1");
        mine.grants[0].params["as"] = json!("dev");
        assert_eq!(mine.repo_acting_as("A/One"), Some("dev"));
        assert_eq!(app.repo_acting_as("a/one"), None);

        let as_app = json!({"repo": "a/one"});
        let as_dev = json!({"repo": "a/one", "as": "dev"});
        assert!(same_identity(&app, Some("a/one"), &as_app).is_ok());
        assert!(same_identity(&mine, Some("a/one"), &as_dev).is_ok());
        let switched = same_identity(&mine, Some("a/one"), &as_app).unwrap_err();
        assert!(
            switched
                .0
                .contains("proposed to act as the GitHub App, but the session now acts as dev"),
            "{}",
            switched.0
        );
        assert!(same_identity(&app, Some("a/one"), &as_dev).is_err());
        assert!(
            same_identity(&mine, None, &json!({})).is_ok(),
            "nothing on a repository"
        );
    }

    /// A kind that proposes on the repository it's given, and logs from inside its preparation as a GitHub client's
    /// audit does.
    struct OnRepo;

    #[async_trait::async_trait]
    impl ProposalKind for OnRepo {
        fn manifest(&self) -> sloppod_contracts::Manifest {
            sloppod_contracts::Manifest::builtin("test")
        }
        fn kind(&self) -> String {
            "test.on_repo".into()
        }
        fn summary(&self, _: &Value) -> String {
            String::new()
        }
        fn tool(&self) -> ToolSpec {
            ToolSpec {
                name: "on_repo".into(),
                description: String::new(),
                input_schema: json!({}),
            }
        }
        async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
            let repo = arguments["repo"].as_str().unwrap_or_default();
            crate::github::record_credential(session.repo_credential(repo));
            tracing::info!("a call made while preparing");
            Ok(json!({"repo": repo}))
        }
        async fn detail(&self, _: &SessionContext, _: &Value) -> Result<Value> {
            Ok(json!({}))
        }
        fn target(&self, payload: &Value) -> Option<String> {
            payload["repo"].as_str().map(str::to_owned)
        }
        async fn execute(&self, _: &SessionContext, _: Uuid, _: &Value) -> Result<Value> {
            Ok(json!({}))
        }
    }

    #[derive(Clone, Default)]
    struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn every_round_s_payload_says_who_it_acts_as_and_its_calls_are_attributed() {
        let mut mine = session(&["a/one"]);
        mine.session_id = "s-1".into();
        mine.grants[0].params["credential"] = json!("c1");
        mine.grants[0].params["as"] = json!("dev");
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);

        // A first round and a later one (request_review) are prepared the same way.
        for _round in 0..2 {
            let payload = prepared(&OnRepo, &mine, json!({"repo": "a/one"}))
                .await
                .unwrap();
            assert_eq!(payload["as"], "dev");
            assert!(
                same_identity(&mine, Some("a/one"), &payload).is_ok(),
                "it can still be carried out"
            );
        }
        let app = prepared(&OnRepo, &session(&["a/one"]), json!({"repo": "a/one"}))
            .await
            .unwrap();
        assert_eq!(app.get("as"), None);

        let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let line = logs
            .lines()
            .find(|l| l.contains("a call made while preparing"))
            .unwrap();
        assert!(
            line.contains("session=s-1") && line.contains("credential=\"c1\""),
            "{line}"
        );
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
        assert!(head_branch(branch, Some("sloppod/00000000-fix+1")).is_err());
        assert!(head_branch(branch, Some("sloppod/00000000-a#b")).is_err());
        assert!(head_branch(branch, Some("sloppod/00000000-a b")).is_err());
        assert_eq!(
            head_branch(branch, Some("sloppod/00000000-v1.2_x")).unwrap(),
            "sloppod/00000000-v1.2_x"
        );
    }

    /// An executor that succeeded, then a database that refuses the outcome a few times: the work is done once, and
    /// its result is what's recorded in the end.
    #[tokio::test(start_paused = true)]
    async fn a_failed_outcome_write_is_retried_not_the_work() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (runs, writes) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let result = json!({"url": "https://github.com/o/r/pull/1"});
        let recorded = execute_then_record(
            Uuid::nil(),
            async {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(result.clone())
            },
            |executed| {
                let attempt = writes.fetch_add(1, Ordering::SeqCst);
                async move {
                    match attempt {
                        0..3 => Err(sqlx::Error::PoolTimedOut),
                        _ => Ok(executed),
                    }
                }
            },
        )
        .await;
        assert_eq!(recorded, Ok(result));
        assert_eq!(writes.load(Ordering::SeqCst), 4);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }
}
