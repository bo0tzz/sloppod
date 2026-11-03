//! `/acp`: the runtime as an ACP agent towards UIs (ADR 9, `docs/api.md`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::link::RpcError;
use crate::session::{UiEvent, session_update};
use crate::users::User;
use crate::{AppState, db, quota};

pub async fn handler(
    State(state): State<AppState>,
    User(user): User,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    ws.max_message_size(64 << 20)
        .on_upgrade(move |socket| run(state, user, agent, socket))
}

/// One UI connection.
struct Conn {
    /// Names the connection in the log, e.g. as the one that cancelled a turn or answered a permission request.
    id: Uuid,
    state: AppState,
    /// Every session this connection touches must be theirs (ADR 23).
    user: String,
    /// Bounded, so a UI that reads slowly slows its replay down rather than having it pile up here.
    out: mpsc::Sender<Value>,
    /// Our outgoing permission requests: UI request id -> (session, permission).
    permissions: Mutex<HashMap<i64, (Uuid, u64)>>,
    next_id: std::sync::atomic::AtomicI64,
    loaded: Mutex<HashMap<Uuid, JoinHandle<()>>>,
    /// Ends the connection: a UI that fell too far behind reconnects and replays rather than missing updates.
    drop_connection: tokio::sync::Notify,
}

impl Conn {
    async fn send(&self, message: Value) {
        let _ = self.out.send(message).await;
    }

    async fn reply(&self, id: Value, result: Result<Value, RpcError>) {
        self.send(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        })
        .await;
    }

    async fn send_permission(&self, session: Uuid, pid: u64, method: String, params: Value) {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.permissions.lock().await.insert(id, (session, pid));
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
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
                    self.send(message).await;
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
                    }))
                    .await;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(%session, skipped = n, "UI fell behind; dropping it so it reloads");
                    self.drop_connection.notify_one();
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// Replays a session; returns the load response and the permission requests still open, which go out after it.
    async fn load(
        self: &Arc<Self>,
        session: Uuid,
    ) -> Result<(Value, Vec<(u64, String, Value)>, Value), RpcError> {
        if db::owned_session(&self.state.db, session, &self.user)
            .await
            .map_err(internal)?
            .is_none()
        {
            return Err(RpcError::new(-32602, "unknown session"));
        }
        // Subscribe before reading the log, so nothing falls between the two; the sequence filter drops overlap.
        let events = self.state.hub.subscribe(session);
        // Up to the end of the log as it is now; what's recorded meanwhile comes from the subscription.
        let until = self.state.hub.last_event(session).await.map_err(internal)?;
        // Only the latest of it; the UI asks for older pages (`_sloppod/history`) as they're scrolled to.
        let latest = self
            .state
            .hub
            .events_before(session, until + 1, HISTORY_PAGE)
            .await
            .map_err(internal)?;
        for (_, update) in &latest {
            self.send(session_update(session, update)).await;
        }
        let replay = self.state.hub.replay(session).await.map_err(internal)?;
        let task = tokio::spawn(self.clone().forward(session, events, until));
        if let Some(old) = self.loaded.lock().await.insert(session, task) {
            old.abort();
        }
        let mut response = replay.agent_state;
        if let Some(response) = response.as_object_mut() {
            response.insert(
                "_meta".into(),
                json!({"sloppod": {"historyBefore": older_than(&latest)}}),
            );
        }
        Ok((response, replay.permissions, replay.queue))
    }

    /// A page of the session's log before `before`, for a UI scrolling back.
    async fn history(&self, session: Uuid, params: &Value) -> Result<Value, RpcError> {
        let before = params["before"]
            .as_i64()
            .ok_or_else(|| RpcError::new(-32602, "`before` is required"))?;
        let page = self
            .state
            .hub
            .events_before(session, before, HISTORY_PAGE)
            .await
            .map_err(internal)?;
        let before = older_than(&page);
        let updates: Vec<Value> = page.into_iter().map(|(_, update)| update).collect();
        Ok(json!({"updates": updates, "before": before}))
    }

    async fn handle_request(self: Arc<Self>, id: Value, method: String, params: Value) {
        let session = params["sessionId"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok());
        let hub = &self.state.hub;
        if let ("session/load" | "session/resume", Some(session)) = (method.as_str(), session) {
            match self.load(session).await {
                Ok((state, permissions, queue)) => {
                    // The queue before the response, so a UI that finished loading knows what's waiting, e.g. to
                    // tell whether a message it sent before reconnecting arrived.
                    self.send(queue).await;
                    self.reply(id, Ok(state)).await;
                    for (pid, method, params) in permissions {
                        self.send_permission(session, pid, method, params).await;
                    }
                }
                Err(e) => self.reply(id, Err(e)).await,
            }
            return;
        }
        // The UI is done with a session (it moved on to another): no more of its events or requests on this connection.
        // What it left open waits in the hub and comes with the next load.
        if let ("session/close", Some(session)) = (method.as_str(), session) {
            if let Some(task) = self.loaded.lock().await.remove(&session) {
                task.abort();
            }
            self.permissions
                .lock()
                .await
                .retain(|_, (s, _)| *s != session);
            self.reply(id, Ok(json!({}))).await;
            return;
        }
        // Loading checks ownership, so sessions a connection loaded are the ones it may act on.
        if let Some(session) = session
            && !self.loaded.lock().await.contains_key(&session)
        {
            self.reply(id, Err(RpcError::new(-32602, "load the session first")))
                .await;
            return;
        }
        let result = match (method.as_str(), session) {
            ("initialize", _) => Ok(self.initialize()),
            ("authenticate", _) => Ok(json!({})),
            ("session/list", _) => self.list().await,
            ("_sloppod/history", Some(session)) => self.history(session, &params).await,
            ("session/new", _) => Err(RpcError::new(
                -32601,
                "create sessions with POST /api/v1/sessions",
            )),
            ("session/prompt", Some(session)) => hub.prompt(session, params).await,
            ("_session/steering", Some(session)) => hub.steer(session, params).await,
            ("_sloppod/steer_queued", Some(session)) => hub.steer_queued(session, params).await,
            ("_sloppod/remove_queued", Some(session)) => hub.remove_queued(session, params).await,
            ("session/set_config_option", Some(session)) => {
                hub.set_config_option(session, params).await
            }
            (_, None) if method.starts_with("session/") || method.starts_with("_session/") => {
                Err(RpcError::new(-32602, "missing or invalid sessionId"))
            }
            _ => Err(RpcError::new(-32601, format!("{method} is not supported"))),
        };
        self.reply(id, result).await;
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
        let mut listed = Vec::new();
        for s in &sessions {
            listed.push(json!({
                "sessionId": s.id,
                "cwd": "/",
                "title": s.title,
                "updatedAt": s.updated_at.to_rfc3339(),
                "_meta": {"sloppod": self.state.hub.session_api(s).await},
            }));
        }
        Ok(json!({"sessions": listed}))
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
        self.state
            .hub
            .answer_permission(session, pid, result, &format!("UI connection {}", self.id))
            .await;
    }
}

/// How many recorded updates a UI gets at once: the latest when it loads a session, then each older page.
const HISTORY_PAGE: i64 = 2000;

/// Where the page before `page` ends, if there may be one: a full page may have more before it.
fn older_than(page: &[(i64, Value)]) -> Option<i64> {
    (page.len() as i64 == HISTORY_PAGE).then(|| page[0].0)
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(-32603, e.to_string())
}

/// Sends the connection each change of one of its owner's sessions, as a `_sloppod/session` notification: the session
/// as the API shows it, `waiting` included. A connection that falls behind is told to reconnect, so it fetches the
/// list again rather than miss one.
async fn forward_sessions(conn: &Conn, mut changes: broadcast::Receiver<Uuid>) {
    loop {
        let id = match changes.recv().await {
            Ok(id) => id,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                return conn.drop_connection.notify_one();
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let row = match db::get_session(&conn.state.db, id).await {
            Ok(Some(row)) if row.owner == conn.user => row,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "reading a changed session to push it");
                continue;
            }
        };
        let session = conn.state.hub.session_api(&row).await;
        conn.send(
            json!({"jsonrpc": "2.0", "method": "_sloppod/session", "params": {"session": session}}),
        )
        .await;
    }
}

async fn run(state: AppState, user: String, agent: String, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out, mut rx) = mpsc::channel::<Value>(64);
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
    let id = Uuid::new_v4();
    tracing::info!(connection = %id, %user, user_agent = %agent, "UI connected");
    let conn = Arc::new(Conn {
        id,
        state,
        user,
        out,
        permissions: Mutex::default(),
        next_id: std::sync::atomic::AtomicI64::new(1),
        loaded: Mutex::default(),
        drop_connection: tokio::sync::Notify::new(),
    });
    // The owner's quota changes, whatever session the connection shows, or none: the usage meters follow them.
    let quotas = {
        let conn = conn.clone();
        let changes = conn.state.quotas.subscribe();
        tokio::spawn(async move {
            let out = conn.out.clone();
            quota::forward(&conn.user, changes, out, || {
                conn.drop_connection.notify_one()
            })
            .await
        })
    };
    // The owner's sessions as they change, whichever one the connection shows, or none: the session list follows them.
    let sessions = {
        let conn = conn.clone();
        let changes = conn.state.hub.changed_sessions();
        tokio::spawn(async move { forward_sessions(&conn, changes).await })
    };
    loop {
        let message = tokio::select! {
            message = stream.next() => message,
            _ = conn.drop_connection.notified() => break,
        };
        let Some(Ok(message)) = message else { break };
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            conn.send(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}))
                .await;
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
                    conn.state
                        .hub
                        .cancel(session, &format!("UI connection {}", conn.id))
                        .await;
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
    quotas.abort();
    sessions.abort();
    writer.abort();
    tracing::info!(connection = %conn.id, "UI disconnected");
}
