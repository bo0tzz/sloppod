//! The runtime's MCP endpoint: tools for proposals, reached from session pods and identified by pod IP.

use std::net::SocketAddr;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{AppState, db, proposals};

pub async fn handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(message): Json<Value>,
) -> Response {
    let Some(session) = state.pods.session_for_ip(&peer.ip().to_string()) else {
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
        "tools/list" => Ok(json!({"tools": tools(&state)})),
        "tools/call" => Ok(call(&state, session, &message["params"]).await),
        _ => Err(json!({"code": -32601, "message": format!("{method} is not supported")})),
    };
    let body = match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    };
    Json(body).into_response()
}

fn tools(state: &AppState) -> Vec<Value> {
    let mut tools: Vec<Value> = state
        .proposals
        .tools()
        .into_iter()
        .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema}))
        .collect();
    tools.push(json!({
        "name": "get_proposal",
        "description": "Check the status of a proposal you made: pending, approved, rejected (with the reviewer's reason), executed (with the result, e.g. the PR URL) or failed.",
        "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
    }));
    tools
}

fn text(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

async fn call(state: &AppState, session: Uuid, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    if name == "get_proposal" {
        let id = arguments["id"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok());
        let proposal = match id {
            Some(id) => db::get_proposal(&state.db, id).await.ok().flatten(),
            None => None,
        };
        return match proposal {
            Some(p) if p.session_id == session => text(
                serde_json::to_string_pretty(&p.to_api()).unwrap_or_default(),
                false,
            ),
            _ => text("no such proposal".into(), true),
        };
    }
    match proposals::propose(state, session, name, arguments).await {
        Ok(proposal) => text(
            format!(
                "Proposal {} created and waiting for human review. Nothing has happened yet; once it's approved sloppod \
                 carries it out. Check its status with get_proposal.",
                proposal.id
            ),
            false,
        ),
        Err(e) => text(format!("Proposal refused: {e}"), true),
    }
}
