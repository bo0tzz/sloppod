//! The runtime's MCP endpoint: tools for proposals, reached from session pods and identified by pod IP.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex as StdMutex;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use sloppod_contracts::SessionRole;
use sloppod_protocol::api::{ProposalStatus, SessionKind};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{AppState, db, proposals, reviews, subsessions, work};

pub async fn handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(message): Json<Value>,
) -> Response {
    let Some(session) = crate::identity::calling_session(&state, &headers, peer.ip()).await else {
        return (StatusCode::FORBIDDEN, "not a session pod").into_response();
    };
    let Some(id) = message.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = message["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": message["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": {"name": "sloppod", "version": state.config.build.as_deref().unwrap_or(env!("CARGO_PKG_VERSION"))},
            "instructions": "sloppod's tools for this session: proposals of outward actions (a person reviews them before sloppod carries them out), questions to the user, what the session is granted, and, for work sessions, sub-sessions. Each tool says what it does.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => match session_kind(&state, session).await {
            Ok(row) => {
                let tools = tools(&state, &row);
                state.tool_streams.listed(session, &tools);
                Ok(json!({"tools": tools}))
            }
            Err(e) => Err(json!({"code": -32603, "message": e})),
        },
        "tools/call" => Ok(call(&state, session, &message["params"]).await),
        _ => Err(json!({"code": -32601, "message": format!("{method} is not supported")})),
    };
    let body = match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    };
    Json(body).into_response()
}

/// What each session's agent was last told its tools are, and the streams it listens on for a change to them (#323).
/// An agent's MCP client fetches the tool list when it connects; a grant that changes the list mid-session would
/// otherwise reach it only at its next start.
#[derive(Default)]
pub struct ToolStreams(StdMutex<HashMap<Uuid, Listed>>);

struct Listed {
    /// A digest of the tool list the session was last given, if it asked.
    tools: Option<u64>,
    changed: broadcast::Sender<()>,
}

impl Listed {
    fn new() -> Self {
        Self {
            tools: None,
            changed: broadcast::channel(4).0,
        }
    }
}

fn digest(tools: &[Value]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    Value::Array(tools.to_vec()).to_string().hash(&mut hasher);
    hasher.finish()
}

impl ToolStreams {
    fn listed(&self, session: Uuid, tools: &[Value]) {
        let mut sessions = self.0.lock().unwrap();
        sessions.entry(session).or_insert_with(Listed::new).tools = Some(digest(tools));
    }

    fn subscribe(&self, session: Uuid) -> broadcast::Receiver<()> {
        let mut sessions = self.0.lock().unwrap();
        sessions
            .entry(session)
            .or_insert_with(Listed::new)
            .changed
            .subscribe()
    }

    /// Whether the session's client was given a list other than `tools`: a change it missed while it had no stream
    /// open, which it hears of when it opens one.
    fn stale(&self, session: Uuid, tools: &[Value]) -> bool {
        let sessions = self.0.lock().unwrap();
        sessions
            .get(&session)
            .and_then(|listed| listed.tools)
            .is_some_and(|given| given != digest(tools))
    }

    /// Forgets a session that's gone.
    pub fn forget(&self, session: Uuid) {
        self.0.lock().unwrap().remove(&session);
    }

    /// Tells the session's agent its tools changed, if they differ from what it was last given. Says whether it did.
    fn changed(&self, session: Uuid, tools: &[Value]) -> bool {
        let mut sessions = self.0.lock().unwrap();
        let Some(listed) = sessions.get_mut(&session) else {
            return false;
        };
        let now = Some(digest(tools));
        if listed.tools.is_none() || listed.tools == now {
            return false;
        }
        // An agent with no stream open doesn't hear of it, and still has the list it was given.
        let sent = listed.changed.send(()).is_ok();
        if sent {
            listed.tools = now;
        }
        sent
    }
}

/// Sends `notifications/tools/list_changed` to the session's agent when its tools are no longer the ones it was
/// given, e.g. after a grant change (#323). Its client then lists them again.
pub async fn tools_changed(state: &AppState, session: Uuid) {
    let tools = current_tools(state, session).await;
    if state.tool_streams.changed(session, &tools) {
        tracing::info!(%session, "told the agent its tools changed");
    }
}

/// The tools the session has now; none if it can't act.
async fn current_tools(state: &AppState, session: Uuid) -> Vec<Value> {
    match session_kind(state, session).await {
        Ok(row) => tools(state, &row),
        Err(_) => Vec::new(),
    }
}

/// `GET /mcp`: the stream the server sends the agent's MCP client notifications on (MCP's streamable HTTP). The only
/// one sent is `notifications/tools/list_changed`.
pub async fn stream(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(session) = crate::identity::calling_session(&state, &headers, peer.ip()).await else {
        return (StatusCode::FORBIDDEN, "not a session pod").into_response();
    };
    // Subscribed before it's checked, so a change in between is either sent on the stream or found stale here.
    let changed = state.tool_streams.subscribe(session);
    let missed = state
        .tool_streams
        .stale(session, &current_tools(&state, session).await);
    let notice = || {
        let notice = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
        Ok::<_, std::convert::Infallible>(
            Event::default().event("message").data(notice.to_string()),
        )
    };
    let notices = futures::stream::unfold(changed, move |mut changed| async move {
        // A lagged receiver missed only repeats of the same notice.
        if let Err(broadcast::error::RecvError::Closed) = changed.recv().await {
            return None;
        }
        Some((notice(), changed))
    });
    let missed = futures::stream::iter(missed.then(notice));
    Sse::new(futures::StreamExt::chain(missed, notices))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Which tools a session gets depends on its kind, so a session that can't be loaded gets none.
async fn session_kind(state: &AppState, session: Uuid) -> Result<db::SessionRow, String> {
    match db::get_session(&state.db, session).await {
        Ok(Some(row)) => match row.not_acting() {
            None => Ok(row),
            Some(why) => Err(format!("this session is {why}, so it has no tools")),
        },
        Ok(None) => Err("this session no longer exists".into()),
        Err(e) => Err(format!("couldn't load the session: {e}")),
    }
}

const LIST_GRANTS: &str = "list_grants";

fn list_grants() -> Value {
    json!({
        "name": LIST_GRANTS,
        "description": "List what this session is granted: the repositories it can read and push to, the APIs it can call and the hosts it can reach.",
        "inputSchema": {"type": "object", "properties": {}},
    })
}

/// A review session's tools (ADR 24): it reports its verdict, and proposes nothing.
fn review_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "submit_review",
            "description": "Submit your review of the change you were asked to review. This is how each round ends; nobody reads chat replies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "verdict": {"type": "string", "enum": ["pass", "changes"], "description": "`changes` when the author should fix something before a human looks, `pass` otherwise; never `pass` with a p0"},
                    "summary": {"type": "string", "description": "Two to four sentences on what the change does and how, and what merging it sets off where you read evidence of it. No verdict, and not a list of the findings"},
                    "findings": {
                        "type": "array",
                        "description": "What should change, one point each; required with `changes`, empty when nothing is worth flagging",
                        "items": {
                            "type": "object",
                            "properties": {
                                "severity": {"type": "string", "enum": ["p0", "p1", "p2"], "description": "p0: must be fixed before this merges. p1: should be fixed. p2: optional polish"},
                                "category": {"type": "string", "enum": ["correctness", "security", "performance", "reliability", "maintainability", "tests"], "description": "What the fix is really about. correctness: wrong behaviour or a broken contract. security: exposure, injection, secrets, unsafe defaults, data loss. performance: cost in time, memory or calls. reliability: error handling, retries, timeouts, concurrency, leaks. maintainability: structure, naming, clarity, stale docs. tests: a test the change needs or one that's wrong"},
                                "title": {"type": "string", "description": "One line, under 80 characters"},
                                "explanation": {"type": "string", "description": "Under what inputs or state it bites, and why it matters"},
                                "path": {"type": "string", "description": "File it's about, relative to the repository root"},
                                "line": {"type": "integer", "description": "Line in the file at the reviewed commit"},
                                "end_line": {"type": "integer", "description": "Last line of the range, when it covers more than one"},
                                "suggested_fix": {"type": "string", "description": "The concrete change to make"},
                                "prior": {"type": "string", "description": "In a later round: the id of the earlier finding this one reports again. Omit for a new finding"},
                            },
                            "required": ["severity", "category", "title", "explanation"],
                        },
                    },
                    "resolved": {"type": "array", "items": {"type": "string"}, "description": "In a later round: ids of the earlier findings that no longer hold"},
                },
                "required": ["verdict", "summary"],
            },
        }),
        list_grants(),
    ]
}

fn tools(state: &AppState, row: &db::SessionRow) -> Vec<Value> {
    let kind = row.kind;
    let context = row.context();
    match kind {
        SessionKind::Review => return review_tools(),
        SessionKind::Utility => return Vec::new(),
        SessionKind::Work => {}
    }
    let mut tools: Vec<Value> = state
        .proposals
        .tools()
        .into_iter()
        .filter(|t| work::agent::shown(&context, &t.name))
        .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema}))
        .collect();
    tools.push(list_grants());
    tools.push(json!({
        "name": "get_proposal",
        "description": "Check the status of a proposal you made: in_review (another model is reviewing it), changes_requested (by that review), pending (waiting for a human), approved, rejected (with the reviewer's reason), withdrawn, executed (with the result, e.g. the PR URL) or failed.",
        "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
    }));
    tools.push(json!({
        "name": "withdraw_proposal",
        "description": "Withdraw a proposal of yours that hasn't been decided yet, e.g. one that's wrong or that you've replaced. Nothing it proposed is carried out, and it's no longer shown to the reviewer.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "reason": {"type": "string", "description": "Why, kept with the proposal"},
            },
            "required": ["id"],
        },
    }));
    tools.push(json!({
        "name": "ask_user",
        "description": "Ask the user one to four multiple-choice questions without waiting for them: the questions are shown in their client, this returns at once, and their answer arrives later as a message starting \"sloppod: the user answered your question\", or \"sloppod: the user dismissed your question without answering it\" if they close it. An open question survives a runtime restart. Carry on with work that doesn't depend on it meanwhile.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string", "description": "The question, ending with a question mark"},
                            "header": {"type": "string", "description": "A short label for it, e.g. \"Auth method\""},
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string", "description": "What choosing it means"},
                                    },
                                    "required": ["label"],
                                },
                            },
                            "multi_select": {"type": "boolean", "default": false},
                        },
                        "required": ["question", "options"],
                    },
                },
            },
            "required": ["questions"],
        },
    }));
    tools.push(json!({
        "name": "request_review",
        "description": "After a review asked for changes to your proposal or to a branch you asked `request_code_review` for, ask for the next round once you've pushed fixes. The proposal is re-pinned to your branch's new head.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "update": {"type": "object", "description": "A new title or description when they no longer describe the change: {title?, body?} for a pull request, {pull_requests: [{title?, body?}]} by position for a stack, {title?, summary?} for a report, {focus} for a code review. What's proposed (repository, branches) can't change; withdraw and propose again for that"},
                "replies": {"type": "string", "description": "Your replies to findings you think don't apply, or that you addressed differently"},
            },
            "required": ["id"],
        },
    }));
    if kind == SessionKind::Work {
        tools.extend(subsessions::tools());
    }
    tools.extend(work::agent::tools(&context));
    tools
}

fn text(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

async fn call(state: &AppState, session: Uuid, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let row = match session_kind(state, session).await {
        Ok(row) => row,
        Err(e) => return text(e, true),
    };
    if !tools(state, &row).iter().any(|t| t["name"] == name) {
        return text(format!("{name} isn't one of this session's tools"), true);
    }
    let result = match name {
        "submit_review" => reviews::submit(state, session, &arguments).await,
        "request_review" => reviews::request(state, session, &arguments).await,
        "ask_user" => ask_user(state, session, &arguments).await,
        LIST_GRANTS => list_grants_text(state, session).await,
        "get_proposal" => get_proposal(state, session, &arguments).await,
        "withdraw_proposal" => withdraw_proposal(state, session, &arguments).await,
        "spawn_session" => subsessions::spawn(state, session, &arguments).await,
        "list_sessions" => subsessions::list(state, session).await,
        "message_session" => subsessions::message(state, session, &arguments).await,
        "park_session" => subsessions::park(state, session, &arguments).await,
        work::agent::LIST | work::agent::UPDATE => {
            work::agent::call(&state.db, &row.context(), name, &arguments).await
        }
        _ => propose(state, session, name, arguments).await,
    };
    match result {
        Ok(message) => text(message, false),
        Err(e) => text(e, true),
    }
}

async fn ask_user(state: &AppState, session: Uuid, arguments: &Value) -> Result<String, String> {
    state
        .hub
        .ask(session, arguments["questions"].clone())
        .await?;
    Ok(
        "Asked. The answer arrives later as a message; carry on with what doesn't depend on it."
            .into(),
    )
}

async fn list_grants_text(state: &AppState, session: Uuid) -> Result<String, String> {
    match db::get_session(&state.db, session).await {
        Ok(Some(row)) => Ok(grant_list(&row.to_api().grants, &row.role())),
        _ => Err("couldn't load the session".into()),
    }
}

fn proposal_id(arguments: &Value) -> Option<Uuid> {
    arguments["id"].as_str().and_then(|s| s.parse().ok())
}

async fn get_proposal(
    state: &AppState,
    session: Uuid,
    arguments: &Value,
) -> Result<String, String> {
    let proposal = match proposal_id(arguments) {
        Some(id) => db::get_proposal(&state.db, id).await.ok().flatten(),
        None => None,
    };
    match proposal {
        Some(p) if p.session_id == session => {
            Ok(serde_json::to_string_pretty(&p.to_api()).unwrap_or_default())
        }
        _ => Err("no such proposal".into()),
    }
}

async fn withdraw_proposal(
    state: &AppState,
    session: Uuid,
    arguments: &Value,
) -> Result<String, String> {
    let id = proposal_id(arguments).ok_or("give the proposal's id")?;
    let reason = arguments["reason"]
        .as_str()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    match db::withdraw_proposal(&state.db, id, session, reason).await {
        Ok(Some(row)) => {
            state.hub.broadcast_proposal(&row.to_api());
            if let Some(review) = row.review_session_id {
                reviews::finish(state, review);
            }
            Ok(format!("Proposal {id} is withdrawn."))
        }
        Ok(None) => Err(
            "no undecided proposal of yours with that id; decided ones can't be withdrawn".into(),
        ),
        Err(e) => Err(format!("couldn't withdraw it: {e}")),
    }
}

async fn propose(
    state: &AppState,
    session: Uuid,
    tool: &str,
    arguments: Value,
) -> Result<String, String> {
    let proposal = proposals::propose(state, session, tool, arguments)
        .await
        .map_err(|e| format!("Proposal refused: {e}"))?;
    // A code review asks no human: it ends with its findings, and nothing is carried out.
    if proposal.kind == proposals::CODE_REVIEW {
        return Ok(proposals::code_review_started(&proposal));
    }
    Ok(match proposal.status {
        ProposalStatus::InReview => format!(
            "Proposal {} created; another model is reviewing it before a human sees it. Its findings, if any, come \
             back to you.",
            proposal.id
        ),
        ProposalStatus::Pending => format!(
            "Proposal {} created and waiting for human review. Nothing has happened yet; once it's approved sloppod \
             carries it out. Check its status with get_proposal.",
            proposal.id
        ),
        _ => format!(
            "Proposal {} was approved by a standing approval and is being carried out; you'll be told how it went. \
             Check its status with get_proposal.",
            proposal.id
        ),
    })
}

fn grant_list(grants: &[sloppod_protocol::api::Grant], role: &SessionRole) -> String {
    let mut lines: Vec<String> = grants
        .iter()
        .map(|g| {
            let until = g
                .expires_at
                .as_ref()
                .map(|at| format!(" (until {at})"))
                .unwrap_or_default();
            format!(
                "- {}: {}{until}",
                g.capability,
                g.description.as_deref().unwrap_or_default()
            )
        })
        .collect();
    let next = role
        .repo_advice()
        .map(|advice| format!("; for another repository, {advice}"))
        .unwrap_or_default();
    lines.push(format!("Anything else is refused{next}."));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_hears_when_its_tools_change_from_what_it_was_given() {
        let streams = ToolStreams::default();
        let session = Uuid::new_v4();
        let before = [json!({"name": "ask_user"})];
        let after = [json!({"name": "ask_user"}), json!({"name": "list_work"})];
        // Not listed yet: it gets the current list when it asks.
        assert!(!streams.changed(session, &after));
        streams.listed(session, &before);
        // No stream open: nobody to tell, and it still has what it was given.
        assert!(!streams.changed(session, &after));
        let mut changed = streams.subscribe(session);
        assert!(!streams.changed(session, &before));
        assert!(streams.changed(session, &after));
        assert!(changed.try_recv().is_ok());
        assert!(!streams.changed(session, &after), "told once");
        assert!(
            streams.changed(session, &before),
            "and again when they change back"
        );
        streams.forget(session);
        assert!(!streams.changed(session, &after));
    }

    #[test]
    fn a_change_missed_while_no_stream_was_open_is_told_when_one_opens() {
        let streams = ToolStreams::default();
        let session = Uuid::new_v4();
        let before = [json!({"name": "ask_user"})];
        let after = [json!({"name": "ask_user"}), json!({"name": "list_work"})];
        streams.listed(session, &before);
        let stream = streams.subscribe(session);
        drop(stream);
        // The grant changes while the stream is closed: nobody hears.
        assert!(!streams.changed(session, &after));
        // Reopened with no further change, it's told of the one it missed, until it lists the tools again.
        let _stream = streams.subscribe(session);
        assert!(streams.stale(session, &after));
        streams.listed(session, &after);
        assert!(!streams.stale(session, &after));
        // Never listed: nothing to be stale.
        assert!(!streams.stale(Uuid::new_v4(), &after));
    }

    #[test]
    fn the_grant_list_points_to_request_repository_only_where_it_exists() {
        assert!(grant_list(&[], &SessionRole::Work).contains("request_repository"));
        let sub = grant_list(&[], &SessionRole::SubSession { parent: "p".into() });
        assert!(!sub.contains("request_repository"), "{sub}");
        assert!(sub.contains("your report"), "{sub}");
        assert_eq!(
            grant_list(&[], &SessionRole::Review),
            "Anything else is refused."
        );
    }
}
