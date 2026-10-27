//! Review sessions (ADR 24): another model reviews a proposal before a human sees it, over up to three rounds.

use std::time::Duration;

use serde_json::{Value, json};
use sloppod_contracts::{Grant, ReviewSubject};
use uuid::Uuid;

use crate::{AppState, api, db, proposals};

/// After this many rounds with changes requested, the proposal goes to the human with the open findings.
pub const MAX_ROUNDS: i32 = 3;
/// The agent review sessions run: a different model from the authoring agent's.
pub const REVIEWER: &str = "codex";

/// Whether `owner` has what the reviewer's model needs.
pub async fn available(state: &AppState, owner: &str) -> anyhow::Result<bool> {
    let Some(reviewer) = state.agents.get(REVIEWER) else {
        return Ok(false);
    };
    Ok(state
        .credentials
        .secret(owner, reviewer.credential().0)
        .await?
        .is_some())
}

/// Starts the next review round of a proposal in review: in a new review session for the first round, or in the
/// one that reviewed the earlier rounds.
pub async fn start(
    state: &AppState,
    proposal: &db::ProposalRow,
    subject: &ReviewSubject,
    replies: Option<&str>,
) -> anyhow::Result<()> {
    let previous = db::reviews(&state.db, proposal.id).await?.pop();
    let round = db::start_review_round(&state.db, proposal.id, &subject.head_sha, replies).await?;
    let prompt = match &previous {
        Some(previous) => next_round_prompt(subject, round, &previous.head_sha, replies),
        None => first_round_prompt(subject),
    };
    if let Some(session) = proposal.review_session_id {
        state.hub.notify(session, prompt).await;
        return Ok(());
    }
    let author = db::get_session(&state.db, proposal.session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the proposing session is gone"))?;
    let id = Uuid::new_v4();
    let grants = [
        Grant::new("codex", json!({})),
        Grant::new("github", json!({})),
        Grant::new("packages", json!({})),
        Grant::new(
            "repo",
            json!({"repo": subject.repo, "access": "r", "branch": subject.head}),
        ),
    ];
    db::insert_session(
        &state.db,
        db::NewSessionRow {
            id,
            owner: &author.owner,
            title: &format!("Review: {}", subject.title),
            custom_title: true,
            pending_prompt: &prompt,
            agent: REVIEWER,
            kind: "review",
            model: None,
            repo: Some(&subject.repo),
            grants: &grants,
        },
    )
    .await?;
    db::set_review_session(&state.db, proposal.id, id).await?;
    state
        .hub
        .record_user_message(id, &json!([{"type": "text", "text": prompt}]), false)
        .await;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the review session vanished"))?;
    api::place(state, &session).await
}

fn first_round_prompt(subject: &ReviewSubject) -> String {
    let ReviewSubject {
        repo,
        base,
        head,
        head_sha,
        title,
        body,
    } = subject;
    format!(
        "Review this pull request on {repo} before a human sees it. Another agent wrote it; you're a second pair of \
         eyes with fresh context.\n\n\
         Title: {title}\n\n{body}\n\n\
         It merges `{head}` at {head_sha} into `{base}`. Your checkout is on `{head}`; \
         `git fetch origin {base} {head} && git diff origin/{base}...{head_sha}` shows the change, and you can read \
         whatever else of the repository you need. Follow its instructions (AGENTS.md, CLAUDE.md), and check the \
         change against them.\n\n\
         Look for bugs, behaviour that doesn't match the description, missing or wrong tests, and docs that should \
         have changed along with the behaviour they describe. Raise what's worth the author's time, not matters of \
         taste. Don't change or push anything.\n\n\
         When you're done, call the `submit_review` tool: verdict `pass` if it's ready for a human, or `changes` \
         with your findings. Nobody reads your chat replies; only `submit_review` counts."
    )
}

fn next_round_prompt(
    subject: &ReviewSubject,
    round: i32,
    previous_sha: &str,
    replies: Option<&str>,
) -> String {
    let ReviewSubject { head, head_sha, .. } = subject;
    let replies = match replies {
        Some(replies) => format!("\n\nTheir replies to your findings:\n\n{replies}"),
        None => String::new(),
    };
    format!(
        "Round {round} of your review: the author has worked on your findings, and `{head}` is now at {head_sha} \
         (`git fetch origin {head} && git diff {previous_sha}..{head_sha}` shows what changed). Check whether your \
         findings are addressed, weigh their replies rather than repeating a point they've answered convincingly, and \
         look over the new commits. Then call `submit_review` again.{replies}"
    )
}

fn findings_text(findings: &Value) -> String {
    findings
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| {
            let place = match (f["path"].as_str(), f["line"].as_u64()) {
                (Some(path), Some(line)) => format!("{path}:{line}: "),
                (Some(path), None) => format!("{path}: "),
                _ => String::new(),
            };
            format!(
                "- [{}] {place}{}",
                f["severity"].as_str().unwrap_or("medium"),
                f["comment"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_findings(findings: &Value) -> Result<(), String> {
    let Some(findings) = findings.as_array() else {
        return Err("findings must be a list".into());
    };
    for finding in findings {
        if finding["comment"]
            .as_str()
            .is_none_or(|c| c.trim().is_empty())
        {
            return Err("every finding needs a comment".into());
        }
    }
    Ok(())
}

/// Records a review session's verdict on the proposal it reviews.
pub async fn submit(state: &AppState, session: Uuid, arguments: &Value) -> Result<String, String> {
    let e = |e: anyhow::Error| e.to_string();
    let proposal = db::reviewed_proposal(&state.db, session)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("this session isn't reviewing a proposal")?;
    if proposal.status != "in_review" {
        return Err(format!(
            "the proposal isn't waiting for a review; it's {}",
            proposal.status
        ));
    }
    let verdict = match arguments["verdict"].as_str() {
        Some(v @ ("pass" | "changes")) => v,
        _ => return Err("verdict must be `pass` or `changes`".into()),
    };
    let Some(summary) = arguments["summary"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Err("give a summary".into());
    };
    let findings = arguments.get("findings").cloned().unwrap_or(json!([]));
    check_findings(&findings)?;
    if verdict == "changes" && findings.as_array().is_some_and(Vec::is_empty) {
        return Err("list the changes you're asking for as findings".into());
    }
    let review = db::finish_review_round(&state.db, proposal.id, verdict, summary, &findings)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no review round is running")?;
    let kind = state
        .proposals
        .get(&proposal.kind)
        .ok_or("the proposal's kind is gone")?
        .clone();
    let about = kind.summary(&proposal.payload);
    let round = review.round;

    if verdict == "changes" && round < MAX_ROUNDS {
        let row = db::move_proposal(
            &state.db,
            proposal.id,
            "in_review",
            "changes_requested",
            None,
        )
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the proposal changed meanwhile")?;
        state.hub.broadcast_proposal(&row.to_api());
        state
            .hub
            .notify(
                proposal.session_id,
                format!(
                    "sloppod: the review of your proposal ({about}) asks for changes (round {round} of \
                     {MAX_ROUNDS}): {summary}\n\n{}\n\nFix what applies and push, then call `request_review` with the \
                     proposal's id ({}) and a reply to any finding you think doesn't apply. Or withdraw it with \
                     `withdraw_proposal`.",
                    findings_text(&findings),
                    proposal.id
                ),
            )
            .await;
        return Ok("Review recorded. You'll be told if the author asks for another round.".into());
    }

    let row = db::move_proposal(&state.db, proposal.id, "in_review", "pending", None)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the proposal changed meanwhile")?;
    state.hub.broadcast_proposal(&row.to_api());
    let notice = if verdict == "pass" {
        format!(
            "sloppod: your proposal ({about}) passed review: {summary} It's waiting for the user now."
        )
    } else {
        format!(
            "sloppod: your proposal ({about}) still had findings after {MAX_ROUNDS} review rounds, so it went to the \
             user with them attached:\n\n{}",
            findings_text(&findings)
        )
    };
    state.hub.notify(proposal.session_id, notice).await;
    end(state, session);
    let author = db::get_session(&state.db, proposal.session_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the proposing session is gone")?;
    proposals::apply_standing_approval(state, &author, kind.as_ref(), row.to_api())
        .await
        .map_err(e)?;
    Ok("Review recorded. That's the end of this review.".into())
}

/// Asks for the next review round of a proposal that had changes requested, re-pinned to its branch's new head.
pub async fn request(state: &AppState, session: Uuid, arguments: &Value) -> Result<String, String> {
    let Some(id) = arguments["id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
    else {
        return Err("give the proposal's id".into());
    };
    let proposal = db::get_proposal(&state.db, id)
        .await
        .map_err(|e| e.to_string())?
        .filter(|p| p.session_id == session)
        .ok_or("no such proposal of yours")?;
    if proposal.status != "changes_requested" {
        return Err(format!(
            "only proposals with changes requested go to another round; this one is {}",
            proposal.status
        ));
    }
    let kind = state
        .proposals
        .get(&proposal.kind)
        .ok_or("the proposal's kind is gone")?
        .clone();
    let author = db::get_session(&state.db, session)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("unknown session")?;
    let payload = kind
        .prepare(&author.context(), proposal.payload.clone())
        .await
        .map_err(|e| e.0)?;
    let subject = kind
        .review_subject(&payload)
        .ok_or("this proposal can't be reviewed")?;
    let replies = arguments["replies"]
        .as_str()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    let last = db::reviews(&state.db, id)
        .await
        .map_err(|e| e.to_string())?
        .pop();
    if replies.is_none() && last.is_some_and(|r| r.head_sha == subject.head_sha) {
        return Err(
            "nothing new: push your fixes first, or reply to the findings you think don't apply"
                .into(),
        );
    }
    let row = db::move_proposal(
        &state.db,
        id,
        "changes_requested",
        "in_review",
        Some(&payload),
    )
    .await
    .map_err(|e| e.to_string())?
    .ok_or("the proposal changed meanwhile")?;
    state.hub.broadcast_proposal(&row.to_api());
    start(state, &row, &subject, replies)
        .await
        .map_err(|e| e.to_string())?;
    Ok("The next review round has started; you'll be told how it goes.".into())
}

/// Deletes a review session once it's done, after its current turn ends.
pub fn end(state: &AppState, session: Uuid) {
    let state = state.clone();
    tokio::spawn(async move {
        for _ in 0..300 {
            if !state.hub.turn_running(session).await {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let result = match db::get_session(&state.db, session).await {
            Ok(Some(row)) => api::remove_session(&state, &row).await,
            Ok(None) => Ok(()),
            Err(e) => Err(e.into()),
        };
        if let Err(e) = result {
            tracing::warn!(%session, error = %e, "deleting a finished review session");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_findings() {
        let findings = json!([
            {"path": "src/a.rs", "line": 3, "severity": "high", "comment": "Off by one."},
            {"path": "README.md", "comment": "Not updated."},
            {"comment": "No tests."},
        ]);
        assert_eq!(
            findings_text(&findings),
            "- [high] src/a.rs:3: Off by one.\n- [medium] README.md: Not updated.\n- [medium] No tests."
        );
    }

    #[test]
    fn checks_findings() {
        assert!(check_findings(&json!([{"path": "a", "comment": "x"}])).is_ok());
        assert!(check_findings(&json!([{"path": "a"}])).is_err());
        assert!(check_findings(&json!({"comment": "x"})).is_err());
    }
}
