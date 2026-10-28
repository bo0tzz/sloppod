//! The runtime's MCP endpoint: tools for proposals, reached from session pods and identified by pod IP.

use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use sloppod_protocol::api::{ProposalStatus, SessionKind};
use uuid::Uuid;

use crate::{AppState, db, proposals, reviews, subsessions};

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
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "sloppod", "version": state.config.build.as_deref().unwrap_or(env!("CARGO_PKG_VERSION"))},
            "instructions": "Tools for outward actions. Each creates a proposal that a human reviews before sloppod carries it out.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => match session_kind(&state, session).await {
            Ok(kind) => Ok(json!({"tools": tools(&state, kind)})),
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

/// Which tools a session gets depends on its kind, so a session that can't be loaded gets none.
async fn session_kind(state: &AppState, session: Uuid) -> Result<SessionKind, String> {
    match db::get_session(&state.db, session).await {
        Ok(Some(row)) => match row.not_acting() {
            None => Ok(row.kind),
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

fn tools(state: &AppState, kind: SessionKind) -> Vec<Value> {
    if kind == SessionKind::Review {
        return review_tools();
    }
    let mut tools: Vec<Value> = state
        .proposals
        .tools()
        .into_iter()
        .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema}))
        .collect();
    tools.push(list_grants());
    tools.push(json!({
        "name": "get_proposal",
        "description": "Check the status of a proposal you made: in_review (another model is reviewing it), changes_requested (by that review), pending (waiting for a human), approved, rejected (with the reviewer's reason), executed (with the result, e.g. the PR URL) or failed.",
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
        "description": "Ask the user one to four multiple-choice questions without waiting for them: the questions are shown in their client, this returns at once, and their answer arrives later as a message starting \"sloppod: the user answered your question\". Carry on with work that doesn't depend on it meanwhile.",
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
        "description": "After a review asked for changes to your proposal, ask for the next round once you've pushed fixes. The proposal is re-pinned to your branch's new head.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "update": {"type": "object", "description": "A new title or description when they no longer describe the change: {title?, body?} for a pull request, {pull_requests: [{title?, body?}]} by position for a stack. What's proposed (repository, branches) can't change; withdraw and propose again for that"},
                "replies": {"type": "string", "description": "Your replies to findings you think don't apply, or that you addressed differently"},
            },
            "required": ["id"],
        },
    }));
    if kind == SessionKind::Work {
        tools.extend(subsessions::tools());
    }
    tools
}

fn text(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

async fn call(state: &AppState, session: Uuid, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let kind = match session_kind(state, session).await {
        Ok(kind) => kind,
        Err(e) => return text(e, true),
    };
    if !tools(state, kind).iter().any(|t| t["name"] == name) {
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
        Ok(Some(row)) => Ok(grant_list(&row.context().grants)),
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
                reviews::end(state, review);
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

fn grant_list(grants: &[sloppod_contracts::Grant]) -> String {
    let mut lines: Vec<String> = grants
        .iter()
        .map(|g| format!("- {}: {}", g.capability, crate::grants::describe(g)))
        .collect();
    lines.push(
        "Anything else is refused. Ask for write access to another repository with request_repository.".into(),
    );
    lines.join("\n")
}
