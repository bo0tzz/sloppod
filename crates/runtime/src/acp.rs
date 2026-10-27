//! `/acp`: the runtime as an ACP agent towards UIs (ADR 9, `docs/api.md`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::link::RpcError;
use crate::session::{UiEvent, session_update};
use crate::users::User;
use crate::{AppState, db};

pub async fn handler(
    State(state): State<AppState>,
    User(user): User,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(64 << 20)
        .on_upgrade(move |socket| run(state, user, socket))
}

/// One UI connection.
struct Conn {
    state: AppState,
    /// Every session this connection touches must be theirs (ADR 23).
    user: String,
    out: mpsc::UnboundedSender<Value>,
    /// Our outgoing permission requests: UI request id -> (session, permission).
    permissions: Mutex<HashMap<i64, (Uuid, u64)>>,
    next_id: std::sync::atomic::AtomicI64,
    loaded: Mutex<HashMap<Uuid, JoinHandle<()>>>,
}

impl Conn {
    fn send(&self, message: Value) {
        let _ = self.out.send(message);
    }

    fn reply(&self, id: Value, result: Result<Value, RpcError>) {
        self.send(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        });
    }

    async fn send_permission(&self, session: Uuid, pid: u64, method: String, params: Value) {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.permissions.lock().await.insert(id, (session, pid));
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    async fn forward(
        self: Arc<Self>,
        session: Uuid,
        mut events: broadcast::Receiver<UiEvent>,
        after: i64,
    ) {
        loop {
            match events.recv().await {
                Ok(UiEvent::Notification { seq, message }) => {
                    if seq.is_some_and(|seq| seq <= after) {
                        continue;
                    }
                    self.send(message);
                }
                Ok(UiEvent::PermissionRequest { id, method, params }) => {
                    self.send_permission(session, id, method, params).await
                }
                Ok(UiEvent::PermissionResolved { id, tool_call_id }) => {
                    let request_id = {
                        let mut permissions = self.permissions.lock().await;
                        let request_id = permissions
                            .iter()
                            .find(|(_, (s, p))| *s == session && *p == id)
                            .map(|(request_id, _)| *request_id);
                        permissions.retain(|_, (s, p)| !(*s == session && *p == id));
                        request_id
                    };
                    self.send(json!({
                        "jsonrpc": "2.0",
                        "method": "_sloppod/permission_resolved",
                        "params": {"sessionId": session, "toolCallId": tool_call_id, "requestId": request_id},
                    }));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(%session, skipped = n, "UI fell behind; it should reload the session");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// Replays a session; returns the load response and the permission requests still open, which go out after it.
    async fn load(
        self: &Arc<Self>,
        session: Uuid,
    ) -> Result<(Value, Vec<(u64, String, Value)>), RpcError> {
        if db::owned_session(&self.state.db, session, &self.user)
            .await
            .map_err(internal)?
            .is_none()
        {
            return Err(RpcError::new(-32602, "unknown session"));
        }
        // Subscribe before reading the log, so nothing falls between the two; the sequence filter drops overlap.
        let events = self.state.hub.subscribe(session);
        let replay = self.state.hub.replay(session).await.map_err(internal)?;
        let after = replay.events.last().map(|(seq, _)| *seq).unwrap_or(0);
        for (_, update) in &replay.events {
            self.send(session_update(session, update));
        }
        let task = tokio::spawn(self.clone().forward(session, events, after));
        if let Some(old) = self.loaded.lock().await.insert(session, task) {
            old.abort();
        }
        Ok((replay.agent_state, replay.permissions))
    }

    async fn handle_request(self: Arc<Self>, id: Value, method: String, params: Value) {
        let session = params["sessionId"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok());
        let hub = &self.state.hub;
        if let ("session/load" | "session/resume", Some(session)) = (method.as_str(), session) {
            match self.load(session).await {
                Ok((state, permissions)) => {
                    self.reply(id, Ok(state));
                    for (pid, method, params) in permissions {
                        self.send_permission(session, pid, method, params).await;
                    }
                }
                Err(e) => self.reply(id, Err(e)),
            }
            return;
        }
        // Loading checks ownership, so sessions a connection loaded are the ones it may act on.
        if let Some(session) = session
            && !self.loaded.lock().await.contains_key(&session)
        {
            self.reply(id, Err(RpcError::new(-32602, "load the session first")));
            return;
        }
        let result = match (method.as_str(), session) {
            ("initialize", _) => Ok(self.initialize()),
            ("authenticate", _) => Ok(json!({})),
            ("session/list", _) => self.list().await,
            ("session/new", _) => Err(RpcError::new(
                -32601,
                "create sessions with POST /api/v1/sessions",
            )),
            ("session/prompt", Some(session)) => hub.prompt(session, params).await,
            ("_session/steering", Some(session)) => hub.steer(session, params).await,
            ("session/set_config_option", Some(session)) => {
                hub.set_config_option(session, params).await
            }
            (_, None) if method.starts_with("session/") || method.starts_with("_session/") => {
                Err(RpcError::new(-32602, "missing or invalid sessionId"))
            }
            _ => Err(RpcError::new(-32601, format!("{method} is not supported"))),
        };
        self.reply(id, result);
    }

    fn initialize(&self) -> Value {
        let agent = self
            .state
            .hub
            .agent_capabilities()
            .unwrap_or_else(|| json!({}));
        let mut capabilities = agent["agentCapabilities"].clone();
        if capabilities.is_null() {
            capabilities = json!({"promptCapabilities": {"image": true, "embeddedContext": true}});
        }
        capabilities["loadSession"] = json!(true);
        let mut meta = agent["_meta"].clone();
        if meta.is_null() {
            meta = json!({});
        }
        meta["steering"] = json!({"supported": true});
        json!({
            "protocolVersion": 1,
            "agentCapabilities": capabilities,
            "agentInfo": {"name": "sloppod", "version": env!("CARGO_PKG_VERSION")},
            "authMethods": [],
            "_meta": meta,
        })
    }

    async fn list(&self) -> Result<Value, RpcError> {
        let sessions = db::list_sessions(&self.state.db, &self.user)
            .await
            .map_err(internal)?;
        Ok(json!({"sessions": sessions.iter().map(|s| json!({
            "sessionId": s.id,
            "cwd": "/",
            "title": s.title,
            "updatedAt": s.updated_at.to_rfc3339(),
            "_meta": {"sloppod": s.to_api()},
        })).collect::<Vec<_>>()}))
    }

    async fn handle_response(&self, message: &Value) {
        let Some(id) = message["id"].as_i64() else {
            return;
        };
        let Some((session, pid)) = self.permissions.lock().await.get(&id).copied() else {
            return;
        };
        let result = match message.get("error") {
            Some(error) => Err(RpcError::new(
                error["code"].as_i64().unwrap_or(-32603),
                error["message"].as_str().unwrap_or("error"),
            )),
            None => Ok(message["result"].clone()),
        };
        self.state.hub.answer_permission(session, pid, result).await;
    }
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(-32603, e.to_string())
}

async fn run(state: AppState, user: String, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out, mut rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if sink
                .send(Message::Text(message.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let conn = Arc::new(Conn {
        state,
        user,
        out,
        permissions: Mutex::default(),
        next_id: std::sync::atomic::AtomicI64::new(1),
        loaded: Mutex::default(),
    });
    while let Some(Ok(message)) = stream.next().await {
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            conn.send(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}));
            continue;
        };
        match (
            message.get("method").and_then(Value::as_str),
            message.get("id"),
        ) {
            (Some(method), Some(id)) => {
                tokio::spawn(conn.clone().handle_request(
                    id.clone(),
                    method.to_owned(),
                    message.get("params").cloned().unwrap_or(json!({})),
                ));
            }
            (Some("session/cancel"), None) => {
                if let Some(session) = message["params"]["sessionId"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
                    && conn.loaded.lock().await.contains_key(&session)
                {
                    conn.state.hub.cancel(session).await;
                }
            }
            (Some(_), None) => {}
            (None, Some(_)) => conn.handle_response(&message).await,
            (None, None) => {}
        }
    }
    for (_, task) in conn.loaded.lock().await.drain() {
        task.abort();
    }
    writer.abort();
}
