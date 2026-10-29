//! A debug bundle for one of a session's turns (#378): what's needed to report a response the model's safety
//! classifier stopped, as one JSON file the user downloads, reads, and sends on themselves. sloppod never sends it.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{AppState, db};

/// How many turns before the chosen one the bundle covers.
pub const TURNS_BEFORE: usize = 3;

/// What the runtime knows about itself, as the bundle records it.
pub struct Runtime<'a> {
    pub version: Option<&'a str>,
    pub build: Option<&'a str>,
}

/// The bundle for the turn `message` (the id of the user message that started it) of `session`; none if the session
/// has no such turn.
pub async fn load(
    state: &AppState,
    session: &db::SessionRow,
    message: &str,
) -> anyhow::Result<Option<Value>> {
    let turns = db::turn_starts(&state.db, session.id).await?;
    let Some(chosen) = turns.iter().position(|(_, _, id)| id == message) else {
        return Ok(None);
    };
    let first = chosen.saturating_sub(TURNS_BEFORE);
    let end = turns.get(chosen + 1).map(|(seq, _, _)| *seq);
    let events = db::events_between(&state.db, session.id, turns[first].0, end).await?;
    let transcripts = db::transcripts(&state.db, session.id).await?;
    let runtime = Runtime {
        version: state.config.version.as_deref(),
        build: state.config.build.as_deref(),
    };
    Ok(Some(build(
        session,
        &turns,
        chosen,
        &events,
        &transcripts,
        &runtime,
        Utc::now(),
    )))
}

/// A turn's bounds: from its first event (and when that was recorded) to the next turn's, if there's one.
struct Bounds {
    from: i64,
    to: Option<i64>,
    started: DateTime<Utc>,
    ended: Option<DateTime<Utc>>,
}

/// One line of an agent's transcript, as Claude Code writes it: JSON with a `timestamp`.
struct Line {
    file: String,
    at: Option<DateTime<Utc>>,
    value: Value,
}

/// Puts the bundle together from what [`load`] read: the turns from `chosen - TURNS_BEFORE` to `chosen`, with their
/// events and the model calls made in them, and the agent's transcript up to the end of `chosen`.
pub fn build(
    session: &db::SessionRow,
    turns: &[(i64, DateTime<Utc>, String)],
    chosen: usize,
    events: &[(i64, DateTime<Utc>, Value)],
    transcripts: &[(String, Vec<u8>)],
    runtime: &Runtime,
    now: DateTime<Utc>,
) -> Value {
    let first = chosen.saturating_sub(TURNS_BEFORE);
    let bounds: Vec<Bounds> = (first..=chosen)
        .map(|k| Bounds {
            from: turns[k].0,
            to: turns.get(k + 1).map(|t| t.0),
            started: turns[k].1,
            ended: turns.get(k + 1).map(|t| t.1),
        })
        .collect();
    let end = bounds.last().and_then(|b| b.ended);

    let mut files: Vec<&(String, Vec<u8>)> = transcripts.iter().collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let lines: Vec<Line> = files
        .iter()
        .flat_map(|(name, content)| {
            String::from_utf8_lossy(content)
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .map(|value| Line {
                    file: name.clone(),
                    at: value["timestamp"]
                        .as_str()
                        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                        .map(|t| t.with_timezone(&Utc)),
                    value,
                })
                .collect::<Vec<_>>()
        })
        .collect();
    let within = |at: Option<DateTime<Utc>>, from: DateTime<Utc>, to: Option<DateTime<Utc>>| {
        at.is_some_and(|at| at >= from && to.is_none_or(|to| at < to))
    };

    let mut versions = Vec::new();
    let mut models = Vec::new();
    let turns_json: Vec<Value> = bounds
        .iter()
        .map(|b| {
            let mut calls: Vec<Value> = Vec::new();
            for line in lines.iter().filter(|l| within(l.at, b.started, b.ended)) {
                if let Some(version) = line.value["version"].as_str() {
                    push_new(&mut versions, version);
                }
                let Some(request) = line.value["requestId"].as_str() else {
                    continue;
                };
                // Claude Code writes a line per content block of a response, each with the response's request id.
                if calls
                    .iter()
                    .any(|c| c["request_id"] == request && c["file"] == line.file.as_str())
                {
                    continue;
                }
                let model = line.value["message"]["model"].as_str();
                if let Some(model) = model {
                    push_new(&mut models, model);
                }
                calls.push(json!({
                    "request_id": request,
                    "timestamp": line.value["timestamp"],
                    "model": model,
                    "message_id": line.value["message"]["id"],
                    "stop_reason": line.value["message"]["stop_reason"],
                    "file": line.file,
                }));
            }
            let events: Vec<Value> = events
                .iter()
                .filter(|(seq, _, _)| *seq >= b.from && b.to.is_none_or(|to| *seq < to))
                .map(|(seq, at, update)| json!({"seq": seq, "at": at, "update": update}))
                .collect();
            let message = turns.iter().find(|t| t.0 == b.from).map(|t| t.2.clone());
            json!({
                "message_id": message,
                "started_at": b.started,
                "ended_at": b.ended,
                "model_calls": calls,
                "events": events,
            })
        })
        .collect();

    let main = session
        .agent_session_id
        .as_deref()
        .map(|id| format!("{id}.jsonl"));
    let transcript: Vec<&Value> = lines
        .iter()
        .filter(|l| main.as_deref() == Some(l.file.as_str()))
        .take_while(|l| match (l.at, end) {
            (Some(at), Some(end)) => at < end,
            _ => true,
        })
        .map(|l| &l.value)
        .collect();

    let mut unavailable = Vec::new();
    if transcripts.is_empty() {
        unavailable.push(format!(
            "The runtime has no transcript of this session's agent ({}), so the model calls' request ids, the \
             agent's version and its own transcript aren't included. Codex keeps its history in SQLite on the \
             session's volume, not in transcript files the runtime keeps (ADR 24).",
            session.agent
        ));
    } else if transcript.is_empty() {
        unavailable.push(
            "The agent's transcript of this conversation wasn't found among its files.".to_owned(),
        );
    }
    let adapter = session
        .agent_state
        .as_ref()
        .map(|s| s["agentInfo"].clone())
        .filter(|info| !info.is_null());
    if adapter.is_none() {
        unavailable.push(
            "The ACP adapter's version: the agent hasn't said it since the runtime started keeping it.".to_owned(),
        );
    }

    let mut bundle = json!({
        "format": "sloppod-debug-bundle/1",
        "generated_at": now,
        "note": "Made for you to read and send on yourself; sloppod doesn't send it anywhere. Credentials never \
                 reach a session's pod, and the placeholders that stand in for them are redacted.",
        "runtime": {"version": runtime.version, "build": runtime.build},
        "session": {
            "id": session.id,
            "agent": session.agent,
            "agent_session_id": session.agent_session_id,
            "model": session.model,
            "created_at": session.created_at,
        },
        "adapter": adapter,
        "agent_versions": versions,
        "models": models,
        "turn": turns[chosen].2,
        "turns": turns_json,
        "transcript": {"file": main, "lines": transcript},
        "unavailable": unavailable,
    });
    redact(&mut bundle);
    bundle
}

fn push_new(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|v| v == value) {
        list.push(value.to_owned());
    }
}

/// Prefixes of credentials a transcript could hold; each is redacted with what follows it up to the next character
/// a token doesn't have. The pod only ever has placeholders (ADR 11), but a bundle leaves sloppod, so it's
/// redacted anyway.
const SECRET_PREFIXES: &[&str] = &[
    "sk-ant-",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "ghr_",
    "github_pat_",
    crate::adapter::PLACEHOLDER,
];

const REDACTED: &str = "[redacted]";

/// Redacts credentials, and the placeholders that stand in for them, wherever they appear in `value`.
pub fn redact(value: &mut Value) {
    match value {
        Value::String(s) => {
            if SECRET_PREFIXES.iter().any(|p| s.contains(p)) {
                *s = redact_text(s);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        Value::Object(map) => map.values_mut().for_each(redact),
        _ => {}
    }
}

fn redact_text(text: &str) -> String {
    let token = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((at, prefix)) = SECRET_PREFIXES
        .iter()
        .filter_map(|p| rest.find(p).map(|at| (at, *p)))
        .min_by_key(|(at, p)| (*at, std::cmp::Reverse(p.len())))
    {
        // A prefix inside a longer word (e.g. `xghp_`) is someone's text, not a token; placeholders are always
        // redacted.
        let starts_word = rest[..at].chars().next_back().is_none_or(|c| !token(c));
        let tail = &rest[at + prefix.len()..];
        let len = tail.find(|c: char| !token(c)).unwrap_or(tail.len());
        out.push_str(&rest[..at]);
        if starts_word || prefix == crate::adapter::PLACEHOLDER {
            out.push_str(REDACTED);
        } else {
            out.push_str(&rest[at..at + prefix.len() + len]);
        }
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// The file name the bundle downloads as.
pub fn file_name(session: Uuid, message: &str) -> String {
    let short = |s: &str| {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect::<String>()
    };
    format!(
        "sloppod-debug-{}-{}.json",
        short(&session.to_string()),
        short(message)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minute: u32) -> DateTime<Utc> {
        format!("2026-10-29T10:{minute:02}:00Z").parse().unwrap()
    }

    fn line(minute: u32, extra: Value) -> String {
        let mut value =
            json!({"timestamp": format!("2026-10-29T10:{minute:02}:30Z"), "version": "2.1.0"});
        for (k, v) in extra.as_object().unwrap() {
            value[k] = v.clone();
        }
        value.to_string()
    }

    #[test]
    fn a_bundle_covers_the_turn_and_the_ones_before_it() {
        let mut session = db::SessionRow::for_test(Uuid::nil(), "dev");
        session.agent_session_id = Some("abc".into());
        session.agent_state =
            Some(json!({"agentInfo": {"name": "claude-agent-acp", "version": "0.97.0"}}));
        let turns: Vec<_> = (0..6)
            .map(|k| (k * 10 + 1, at(k as u32 * 10), format!("m{k}")))
            .collect();
        let events: Vec<_> = (0..60)
            .map(|seq| {
                (
                    seq + 1,
                    at(seq as u32),
                    json!({"sessionUpdate": "agent_message_chunk"}),
                )
            })
            .collect();
        let assistant = |minute, request: &str| {
            line(
                minute,
                json!({"type": "assistant", "requestId": request,
                       "message": {"id": format!("msg_{request}"), "model": "claude-opus-5-5", "stop_reason": "refusal"}}),
            )
        };
        let content = [
            line(0, json!({"type": "user"})),
            assistant(5, "req_early"),
            assistant(42, "req_a"),
            assistant(42, "req_a"),
            line(43, json!({"type": "user", "message": {"content": "token sk-ant-oat01-sloppod-placeholder"}})),
            assistant(45, "req_b"),
            assistant(52, "req_later"),
        ]
        .join("\n");
        let transcripts = vec![("abc.jsonl".to_owned(), content.into_bytes())];
        let runtime = Runtime {
            version: Some("0.1.2"),
            build: None,
        };
        let bundle = build(&session, &turns, 4, &events, &transcripts, &runtime, at(59));

        assert_eq!(bundle["turn"], "m4");
        let turns = bundle["turns"].as_array().unwrap();
        assert_eq!(
            turns
                .iter()
                .map(|t| t["message_id"].clone())
                .collect::<Vec<_>>(),
            [json!("m1"), json!("m2"), json!("m3"), json!("m4")]
        );
        assert_eq!(turns[3]["events"].as_array().unwrap().len(), 10);
        let calls = &turns[3]["model_calls"];
        assert_eq!(
            calls.as_array().unwrap().len(),
            2,
            "one per request: {calls}"
        );
        assert_eq!(calls[0]["request_id"], "req_a");
        assert_eq!(calls[0]["stop_reason"], "refusal");
        assert_eq!(calls[1]["request_id"], "req_b");
        assert_eq!(bundle["models"], json!(["claude-opus-5-5"]));
        assert_eq!(bundle["agent_versions"], json!(["2.1.0"]));
        assert_eq!(bundle["adapter"]["version"], "0.97.0");
        // The transcript runs up to the end of the chosen turn, and the placeholder is redacted.
        let transcript = bundle["transcript"]["lines"].as_array().unwrap();
        assert_eq!(transcript.len(), 6);
        assert_eq!(transcript[4]["message"]["content"], "token [redacted]");
        assert_eq!(bundle["unavailable"], json!([]));
    }

    #[test]
    fn a_session_without_a_transcript_says_what_is_missing() {
        let mut session = db::SessionRow::for_test(Uuid::nil(), "dev");
        session.agent = "codex".into();
        let turns = vec![(1, at(0), "m0".to_owned())];
        let runtime = Runtime {
            version: None,
            build: None,
        };
        let bundle = build(&session, &turns, 0, &[], &[], &runtime, at(1));
        let unavailable = bundle["unavailable"].as_array().unwrap();
        assert!(unavailable[0].as_str().unwrap().contains("request ids"));
        assert!(unavailable[1].as_str().unwrap().contains("adapter"));
        assert_eq!(bundle["turns"][0]["ended_at"], Value::Null);
    }

    #[test]
    fn redacts_tokens_and_placeholders() {
        let mut value = json!({"a": ["Bearer sloppod-placeholder", "ghp_abc123 and github_pat_X_y-z.", "xghp_ok"]});
        redact(&mut value);
        assert_eq!(
            value,
            json!({"a": ["Bearer [redacted]", "[redacted] and [redacted].", "xghp_ok"]})
        );
    }
}
