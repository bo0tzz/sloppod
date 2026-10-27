//! Proposals (ADR 3): the kinds an agent can propose, and proposing, deciding and carrying them out.

mod issues;
mod pull_request;
mod repo_access;

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use sloppod_contracts::{Error, ProposalKind, RepoGrant, Result, SessionContext, ToolSpec};
use sloppod_protocol::api::{Proposal, ProposalStatus, ReviewPolicy};
use uuid::Uuid;

use crate::{AppState, db, reviews};

pub use issues::Issues;
pub use pull_request::{PullRequest, Stack};
pub use repo_access::{RepoAccess, grant_repo, repo_granted};

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
pub fn is_session_branch(session_branch: &str, branch: &str) -> bool {
    branch == session_branch
        || branch
            .strip_prefix(session_branch)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|name| !name.is_empty())
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
    let required = session.review == ReviewPolicy::Required;
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
        return apply_standing_approval(state, &session, kind.as_ref(), proposal)
            .await
            .map_err(|e| e.to_string());
    };
    if let Err(e) = reviews::start(state, &row, &subject, None).await {
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
            ProposalStatus::Approved,
            kind.edit(&row.payload, &edits)
                .map_err(|e| anyhow::anyhow!(e.0))?,
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
        anyhow::bail!("proposal {id} was already decided");
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
        Ok(result) => {
            db::finish_proposal(&state.db, id, ProposalStatus::Executed, Some(&result), None)
                .await?
        }
        Err(e) => {
            db::finish_proposal(&state.db, id, ProposalStatus::Failed, None, Some(&e.0)).await?
        }
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
    use sloppod_contracts::Grant;

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
}
