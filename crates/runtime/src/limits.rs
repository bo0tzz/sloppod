//! Usage limits: telling a turn that failed on the account's usage limit from other failures, carrying on with the
//! next account on the session's grant (#252), and when a session whose accounts are all limited tries again (#251).
//! What the agents report about the limits themselves is kept by `quota` (#140).

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use sloppod_contracts::SessionContext;
use sqlx::PgPool;

use crate::credentials::{Account, CHATGPT, CredentialRow, Credentials};
use crate::link::RpcError;
use crate::quota::{self, PROBE_AFTER};
use crate::{AppState, db};

/// After the reported reset, so the first request after it isn't refused by a clock a little behind.
const MARGIN: Duration = Duration::minutes(1);

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

/// What a session whose turn hit its usage limit does next.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// Another account on its grant isn't at its limit: the session carries on at once with it, told this (#252).
    Fallback(String),
    /// Every account on its grant is: it waits for the earliest reset, if one is known (#251).
    Wait(Option<DateTime<Utc>>),
}

/// The session's agent's kind of account, and the session, for looking up its accounts.
async fn of_session(state: &AppState, session: Uuid) -> Option<(String, SessionContext)> {
    let row = db::get_session(&state.db, session).await.ok()??;
    let kind = state.agents.get(&row.agent)?.credential().kind;
    Some((kind, row.context()))
}

/// How long the limit of a session's accounts lasts.
enum Lasts {
    /// One of them isn't limited.
    Not,
    /// Until the earliest known reset of theirs.
    Until(DateTime<Utc>),
    /// No reset is known.
    Unknown,
}

async fn lasts(
    state: &AppState,
    kind: &str,
    session: &SessionContext,
    now: DateTime<Utc>,
) -> Option<Lasts> {
    let accounts = state.credentials.accounts(session, kind).await.ok()?;
    let mut earliest: Option<DateTime<Utc>> = None;
    for account in accounts {
        let quota = quota::for_credential(&state.db, account.id, now)
            .await
            .ok()?;
        match quota.filter(|q| q.limited) {
            None => return Some(Lasts::Not),
            Some(quota) => {
                if let Some(reset) = quota.resets_at.as_deref().and_then(|r| r.parse().ok()) {
                    earliest = Some(earliest.map_or(reset, |e: DateTime<Utc>| e.min(reset)));
                }
            }
        }
    }
    Some(earliest.map_or(Lasts::Unknown, Lasts::Until))
}

/// When a session waiting for its usage limit carries on: now if one of its accounts isn't limited, else just after
/// the earliest known reset of theirs; `None` when none is known.
async fn wait_for(
    state: &AppState,
    kind: &str,
    session: &SessionContext,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match lasts(state, kind, session, now).await? {
        Lasts::Not => Some(now),
        Lasts::Until(reset) => Some(wait_until(Some(reset), now)),
        Lasts::Unknown => None,
    }
}

/// The notice a session that moved to another account carries on with.
fn fallback_notice(kind: &str, from: &CredentialRow, to: &CredentialRow) -> String {
    let noun = match kind {
        CHATGPT => "ChatGPT login",
        _ => "Claude account",
    };
    format!(
        "sloppod: this session's {noun} {} hit its usage limit; carrying on with {}. Carry on where you left off.",
        from.name, to.name
    )
}

/// After a session's turn failed on its usage limit: the account it used then, kept as limited, and the next on its
/// grant that isn't limited, if there is one. The account used is the one last attached to its requests, not the one
/// that would be chosen now, which a quota report from that turn may already have moved on.
pub async fn fall_back(
    credentials: &Credentials,
    db: &PgPool,
    session: &SessionContext,
    kind: &str,
    now: DateTime<Utc>,
) -> sqlx::Result<(Option<CredentialRow>, Option<CredentialRow>)> {
    let Some(used) = credentials.used(session, kind).await? else {
        return Ok((None, None));
    };
    quota::mark_limited(db, used.id, now).await?;
    let next = match credentials.account(session, kind).await? {
        Account::Found(next)
            if next.id != used.id && !quota::is_limited(db, next.id, now).await? =>
        {
            Some(next)
        }
        _ => None,
    };
    Ok((Some(used), next))
}

/// A session's turn hit its usage limit (#251, #252). The account it used is kept as limited, and if another account
/// on its grant isn't, the session carries on with that one at once: the authorizer attaches it from the next
/// request. Otherwise it waits for the earliest reset, and a review round it works on stops running out of time from
/// now, kept at once so a restart doesn't lose the wait.
pub async fn hit(state: &AppState, session: Uuid) -> Next {
    let now = Utc::now();
    let Some((kind, context)) = of_session(state, session).await else {
        return wait(state, session, None).await;
    };
    match fall_back(&state.credentials, &state.db, &context, &kind, now).await {
        Ok((Some(used), next)) => {
            quota::announce(state, &used).await;
            if let Some(next) = next {
                tracing::info!(%session, from = %used.id, to = %next.id, "the session's account hit its usage limit; it carries on with the next");
                return Next::Fallback(fallback_notice(&kind, &used, &next));
            }
        }
        Ok((None, _)) => {}
        Err(e) => tracing::warn!(%session, error = %e, "finding an account to fall back to"),
    }
    let reset = match lasts(state, &kind, &context, now).await {
        Some(Lasts::Until(reset)) => Some(reset),
        _ => None,
    };
    wait(state, session, reset).await
}

async fn wait(state: &AppState, session: Uuid, reset: Option<DateTime<Utc>>) -> Next {
    if let Err(e) = db::pause_reviewer(&state.db, session, Utc::now()).await {
        tracing::warn!(%session, error = %e, "pausing the review a session works on");
    }
    Next::Wait(reset)
}

/// A session's model grant has other accounts, or in another order (ADR 20, 29): a session waiting for its usage
/// limit waits for its accounts' earliest reset instead, and carries on now if one of them isn't limited.
pub async fn switched(state: &AppState, session: Uuid) {
    let Some((kind, context)) = of_session(state, session).await else {
        return;
    };
    if let Some(until) = wait_for(state, &kind, &context, Utc::now()).await {
        state.hub.move_limit(session, until).await;
    }
}

/// Keeps the sessions waiting for a usage limit in step with their accounts' quotas: one whose accounts are all
/// limited until known times waits until just after the earliest, and one with an account that isn't limited any
/// more carries on now.
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
            let waiting = match db::limited_sessions(&state.db).await {
                Ok(waiting) => waiting,
                Err(e) => {
                    tracing::warn!(error = %e, "listing sessions waiting for their usage limit");
                    continue;
                }
            };
            for (id, _) in waiting {
                let Some((kind, context)) = of_session(&state, id).await else {
                    continue;
                };
                if context.owner != change.owner {
                    continue;
                }
                let uses =
                    state
                        .credentials
                        .accounts(&context, &kind)
                        .await
                        .is_ok_and(|accounts| {
                            accounts.iter().any(|a| a.id == change.quota.credential_id)
                        });
                if !uses {
                    continue;
                }
                if let Some(until) = wait_for(&state, &kind, &context, Utc::now()).await {
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
