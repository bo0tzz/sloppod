//! Usage limits: telling a turn that failed on the account's usage limit from other failures, and when a session
//! that hit one tries again (#251). What the agents report about the limits themselves is kept by `quota` (#140).

use chrono::{DateTime, Duration, Utc};

use crate::link::RpcError;

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
