//! Usage limits: telling a turn that failed on the account's usage limit from other failures, and when a session
//! that hit one tries again (#251). What the agents report about the limits themselves is kept by `quota` (#140).

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::credentials::{Account, CredentialRow};
use crate::link::RpcError;
use crate::{AppState, db, quota};

/// After the reported reset, so the first request after it isn't refused by a clock a little behind.
const MARGIN: Duration = Duration::minutes(1);
/// How long a session waits before trying again when nothing says when the limit resets.
const PROBE_AFTER: Duration = Duration::minutes(30);

/// How claude-agent-acp's synthetic usage-limit messages start (the Claude Agent SDK's
/// `USAGE_LIMIT_ERROR_PREFIXES`) that reset in time, for when the failed prompt's error carries no kind.
const CLAUDE_LIMIT_PREFIXES: [&str; 2] = ["You've hit your", "You've reached your"];

/// Whether a turn failed because the account is at its usage limit, which resets by itself in time: claude-agent-acp
/// fails the prompt with `data.errorKind` `rate_limit` (its text, e.g. "You've hit your limit · resets 3pm", in the
/// message), codex-acp with `data.codexErrorInfo` `usageLimitExceeded`.
pub fn is_usage_limit(error: &RpcError) -> bool {
    let data = error.data.as_ref();
    let kind = |key: &str| data.and_then(|d| d[key].as_str());
    if kind("errorKind") == Some("rate_limit")
        || kind("codexErrorInfo") == Some("usageLimitExceeded")
    {
        return true;
    }
    let text = error
        .message
        .strip_prefix("Internal error: ")
        .unwrap_or(&error.message);
    CLAUDE_LIMIT_PREFIXES.iter().any(|p| text.starts_with(p))
}

/// When a session that just hit its limit tries again: just after `reset`, when the account's limit is known to
/// reset then, otherwise after a while.
pub fn wait_until(reset: Option<DateTime<Utc>>, now: DateTime<Utc>) -> DateTime<Utc> {
    reset
        .filter(|reset| *reset > now)
        .map_or(now + PROBE_AFTER, |reset| reset + MARGIN)
}

/// The credential a session's agent uses: the account of the kind its agent needs that its grant names (ADR 29).
async fn credential(state: &AppState, session: &db::SessionRow) -> Option<CredentialRow> {
    let kind = state.agents.get(&session.agent)?.credential().kind;
    match state
        .credentials
        .account(&session.context(), &kind)
        .await
        .ok()?
    {
        Account::Found(row) => Some(row),
        _ => None,
    }
}

/// A session's turn hit its usage limit: a review round it works on stops running out of time from now, kept at
/// once so a restart doesn't lose the wait. Says when the limit resets, if that's known.
pub async fn hit(state: &AppState, session: Uuid) -> Option<DateTime<Utc>> {
    if let Err(e) = db::pause_reviewer(&state.db, session, Utc::now()).await {
        tracing::warn!(%session, error = %e, "pausing the review a session works on");
    }
    reset(state, session).await
}

/// When the limit the session's credential is at resets, if its quota says it's at one.
pub async fn reset(state: &AppState, session: Uuid) -> Option<DateTime<Utc>> {
    let row = db::get_session(&state.db, session).await.ok()??;
    let credential = credential(state, &row).await?;
    let quota = quota::for_credential(&state.db, credential.id, Utc::now())
        .await
        .ok()??;
    quota.limited.then_some(())?;
    quota.resets_at?.parse().ok()
}

/// A session's model grant was switched to another account (ADR 20, 29): a session waiting for the old account's
/// limit waits for the new one's instead, and carries on now if the new one isn't at its limit.
pub async fn switched(state: &AppState, session: Uuid) {
    let Ok(Some(row)) = db::get_session(&state.db, session).await else {
        return;
    };
    let now = Utc::now();
    let quota = match credential(state, &row).await {
        Some(credential) => quota::for_credential(&state.db, credential.id, now)
            .await
            .ok()
            .flatten(),
        None => return,
    };
    let until = match quota.filter(|q| q.limited) {
        None => now,
        Some(quota) => match quota.resets_at.as_deref().and_then(|r| r.parse().ok()) {
            Some(reset) => wait_until(Some(reset), now),
            // Limited with no reset known: it waits as it was.
            None => return,
        },
    };
    state.hub.move_limit(session, until).await;
}

/// Keeps the sessions waiting for a usage limit in step with their credentials' quotas: one whose credential
/// is limited until a known time waits until just after it, and one whose credential isn't limited any more
/// carries on now.
pub fn follow_quotas(state: &AppState) {
    let state = state.clone();
    let mut changes = state.quotas.subscribe();
    tokio::spawn(async move {
        loop {
            let change = match changes.recv().await {
                Ok(change) => change,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            let now = Utc::now();
            let until = if change.quota.limited {
                let reset = change
                    .quota
                    .resets_at
                    .as_deref()
                    .and_then(|r| r.parse().ok());
                match reset {
                    Some(reset) => wait_until(Some(reset), now),
                    None => continue,
                }
            } else {
                now
            };
            let waiting = match db::limited_sessions(&state.db).await {
                Ok(waiting) => waiting,
                Err(e) => {
                    tracing::warn!(error = %e, "listing sessions waiting for their usage limit");
                    continue;
                }
            };
            for (id, _) in waiting {
                let Ok(Some(row)) = db::get_session(&state.db, id).await else {
                    continue;
                };
                if row.owner != change.owner {
                    continue;
                }
                if credential(&state, &row)
                    .await
                    .is_some_and(|c| c.id == change.quota.credential_id)
                {
                    state.hub.move_limit(id, until).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn error(message: &str, data: Option<Value>) -> RpcError {
        RpcError {
            code: -32603,
            message: message.into(),
            data,
        }
    }

    #[test]
    fn tells_usage_limits_from_other_failures() {
        assert!(is_usage_limit(&error(
            "Internal error: You've hit your limit · resets 3pm (UTC)",
            Some(json!({"errorKind": "rate_limit"}))
        )));
        assert!(is_usage_limit(&error(
            "Internal error: You've hit your session limit · resets 3pm (UTC)",
            None
        )));
        assert!(is_usage_limit(&error(
            "Internal error",
            Some(
                json!({"message": "You've hit your usage limit.", "codexErrorInfo": "usageLimitExceeded"})
            )
        )));
        assert!(!is_usage_limit(&error(
            "Internal error: API Error: 529 Overloaded",
            Some(json!({"errorKind": "overloaded"}))
        )));
        assert!(!is_usage_limit(&error("upstream 503", None)));
    }

    #[test]
    fn waits_for_a_known_reset_or_tries_again_later() {
        let now = Utc::now();
        assert_eq!(wait_until(None, now), now + PROBE_AFTER);
        let reset = now + Duration::hours(2);
        assert_eq!(wait_until(Some(reset), now), reset + MARGIN);
        // A reset that's passed doesn't hold the session up.
        let later = now + Duration::hours(3);
        assert_eq!(wait_until(Some(reset), later), later + PROBE_AFTER);
    }
}
