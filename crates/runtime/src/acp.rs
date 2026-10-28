//! `/acp`: the runtime as an ACP agent towards UIs (ADR 9, `docs/api.md`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
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
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_allowed(&headers, &state.config.allowed_origins) {
        return (
            StatusCode::FORBIDDEN,
            "sloppod: /acp can only be opened from sloppod's own pages",
        )
            .into_response();
    }
    ws.max_message_size(64 << 20)
        .on_upgrade(move |socket| run(state, user, socket))
}

/// Whether a browser may open `/acp` from where this request comes from. A browser always says which page opens a
/// WebSocket (`Origin`), and the proxy in front authenticates it with whatever it sends along, such as a cookie, so a
/// page on another site must not get a connection as the user. A request without `Origin` doesn't come from a page,
/// e.g. `sloppodctl`.
fn origin_allowed(headers: &HeaderMap, allowed: &[String]) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if allowed
        .iter()
        .any(|a| a.trim_end_matches('/').eq_ignore_ascii_case(origin))
    {
        return true;
    }
    // The host the page was served from: what the browser asked for, as the proxy saw it.
    let Some((_, host)) = origin.split_once("://") else {
        return false;
    };
    ["x-forwarded-host", "host"]
        .iter()
        .filter_map(|name| headers.get(*name)?.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|served| served.trim().eq_ignore_ascii_case(host))
}

/// One UI connection.
struct Conn {
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
        let mut after = 0;
        while after < until {
            let page = self
                .state
                .hub
                .events(session, after, REPLAY_PAGE)
                .await
                .map_err(internal)?;
            let Some((last, _)) = page.last() else { break };
            after = *last;
            for (_, update) in page.iter().filter(|(seq, _)| *seq <= until) {
                self.send(session_update(session, update)).await;
            }
        }
        let after = until;
        let replay = self.state.hub.replay(session).await.map_err(internal)?;
        let task = tokio::spawn(self.clone().forward(session, events, after));
        if let Some(old) = self.loaded.lock().await.insert(session, task) {
            old.abort();
        }
        Ok((replay.agent_state, replay.permissions, replay.queue))
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
            ("session/new", _) => Err(RpcError::new(
                -32601,
                "create sessions with POST /api/v1/sessions",
            )),
            ("session/prompt", Some(session)) => hub.prompt(session, params).await,
            ("_session/steering", Some(session)) => hub.steer(session, params).await,
            ("_sloppod/steer_queued", Some(session)) => hub.steer_queued(session, params).await,
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

/// How many recorded updates a replay reads at once.
const REPLAY_PAGE: i64 = 500;

fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(-32603, e.to_string())
}

async fn run(state: AppState, user: String, socket: WebSocket) {
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
    let conn = Arc::new(Conn {
        state,
        user,
        out,
        permissions: Mutex::default(),
        next_id: std::sync::atomic::AtomicI64::new(1),
        loaded: Mutex::default(),
        drop_connection: tokio::sync::Notify::new(),
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(*name, value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn only_sloppods_own_pages_open_acp() {
        let none: &[String] = &[];
        // Its own pages, directly or through a proxy.
        assert!(origin_allowed(
            &headers(&[
                ("origin", "https://sloppod.example.com"),
                ("host", "sloppod.example.com")
            ]),
            none
        ));
        assert!(origin_allowed(
            &headers(&[
                ("origin", "https://sloppod.example.com"),
                ("host", "sloppod.slop.svc:8080"),
                ("x-forwarded-host", "sloppod.example.com"),
            ]),
            none
        ));
        assert!(origin_allowed(
            &headers(&[
                ("origin", "http://localhost:18080"),
                ("host", "localhost:18080")
            ]),
            none
        ));
        // Not from a page at all.
        assert!(origin_allowed(
            &headers(&[("host", "sloppod.example.com")]),
            none
        ));
        // Another site's page.
        assert!(!origin_allowed(
            &headers(&[
                ("origin", "https://evil.example"),
                ("host", "sloppod.example.com")
            ]),
            none
        ));
        assert!(!origin_allowed(
            &headers(&[
                ("origin", "https://sloppod.example.com.evil.example"),
                ("host", "sloppod.example.com")
            ]),
            none
        ));
        assert!(!origin_allowed(
            &headers(&[("origin", "null"), ("host", "sloppod.example.com")]),
            none
        ));
        // Unless it's allowed.
        assert!(origin_allowed(
            &headers(&[
                ("origin", "https://ui.example.com"),
                ("host", "sloppod.example.com")
            ]),
            &["https://ui.example.com/".to_owned()]
        ));
    }
}
