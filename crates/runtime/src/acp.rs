//! `/acp`: the runtime as an ACP agent towards UIs (ADR 9, `docs/api.md`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::link::RpcError;
use crate::session::{Hub, UiEvent, event_time, session_update_at};
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
        after: Option<i64>,
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
        let (latest, resumed) = replay_page(&self.state.hub, session, until, after)
            .await
            .map_err(internal)?;
        for (seq, at, update) in &latest {
            self.send(session_update_at(session, *seq, *at, update))
                .await;
        }
        let replay = self.state.hub.replay(session).await.map_err(internal)?;
        let task = tokio::spawn(self.clone().forward(session, events, until));
        if let Some(old) = self.loaded.lock().await.insert(session, task) {
            old.abort();
        }
        let mut response = replay.agent_state;
        if let Some(response) = response.as_object_mut() {
            // Resumed: what was sent begins where the UI's own copy ends, so the boundary for older pages is still
            // the one it holds, not the one a page would give it (#449).
            let meta = if resumed {
                json!({"resumed": true})
            } else {
                json!({"resumed": false, "historyBefore": older_than(&latest)})
            };
            response.insert("_meta".into(), json!({"sloppod": meta}));
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
        let times: Vec<String> = page.iter().map(|(_, at, _)| event_time(*at)).collect();
        // Each update's own sequence number, in step with them and their times: one sequence serves every session, so a
        // session's are not consecutive, and a UI joining a page onto what it has needs the real ones (#93).
        let seqs: Vec<i64> = page.iter().map(|(seq, _, _)| *seq).collect();
        let updates: Vec<Value> = page.into_iter().map(|(_, _, update)| update).collect();
        Ok(json!({"updates": updates, "at": times, "seqs": seqs, "before": before}))
    }

    async fn handle_request(self: Arc<Self>, id: Value, method: String, params: Value) {
        let session = params["sessionId"]
            .as_str()
            .and_then(|s| s.parse::<Uuid>().ok());
        let hub = &self.state.hub;
        if let ("session/load" | "session/resume", Some(session)) = (method.as_str(), session) {
            // How far the UI's own copy goes (`_meta.sloppod.after`), when it has one: it's sent only what it missed
            // rather than the latest page again (#449).
            let after = params["_meta"]["sloppod"]["after"].as_i64();
            match self.load(session, after).await {
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
            ("session/prompt", Some(session)) => {
                // A model the user picked that needs the agent started again is moved onto first (#461), for a new
                // message: one sent again is answered as before, and moves nothing.
                let moved = match hub.is_new_prompt(session, &params).await {
                    Ok(true) => crate::endpoint::apply_pending(&self.state, session).await,
                    Ok(false) => Ok(()),
                    Err(e) => Err(e),
                };
                match moved {
                    Ok(()) => hub.prompt(session, params).await,
                    Err(e) => Err(e),
                }
            }
            ("_session/steering", Some(session)) => hub.steer(session, params).await,
            ("_sloppod/steer_queued", Some(session)) => hub.steer_queued(session, params).await,
            ("_sloppod/remove_queued", Some(session)) => hub.remove_queued(session, params).await,
            ("session/set_config_option", Some(session)) => {
                // To or from an endpoint's model, the runtime moves the session itself, at its next message (#461).
                let endpoint = match (params["configId"].as_str(), params["value"].as_str()) {
                    (Some("model"), Some(value)) => {
                        crate::endpoint::choose(&self.state, session, value).await
                    }
                    _ => None,
                };
                match endpoint {
                    Some(result) => result,
                    None => hub.set_config_option(session, params).await,
                }
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
fn older_than(page: &[(i64, DateTime<Utc>, Value)]) -> Option<i64> {
    (page.len() as i64 == HISTORY_PAGE).then(|| page[0].0)
}

/// What a load sends: only what came after the sequence the UI already has, when the log still holds that point and
/// what came after it fits in a page; otherwise the latest page, whole. Whether it's the former, so the UI knows to
/// join the updates onto what it has rather than start over — and that the boundary for older pages is still its own
/// (#449). A page is bounded by `until`, what the log held when the connection subscribed, so nothing is sent twice.
async fn replay_page(
    hub: &Hub,
    session: Uuid,
    until: i64,
    after: Option<i64>,
) -> Result<(Vec<(i64, DateTime<Utc>, Value)>, bool), sqlx::Error> {
    if let Some(after) = after
        // A sequence the log doesn't hold — an older one that was dropped, or one that was never this session's —
        // can't be resumed from: what lies between it and the log's start is gone (#449).
        && hub.has_event(session, after).await?
    {
        let missed = hub
            .events_after(session, after, until + 1, HISTORY_PAGE + 1)
            .await?;
        // Further behind than a page: the UI is better served by the latest page than by a catch-up of any length.
        if missed.len() as i64 <= HISTORY_PAGE {
            return Ok((missed, true));
        }
    }
    let latest = hub.events_before(session, until + 1, HISTORY_PAGE).await?;
    Ok((latest, false))
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
    // The owner's spend on endpoint models, likewise (#461).
    let spend = {
        let conn = conn.clone();
        let changes = conn.state.spending.subscribe();
        tokio::spawn(async move {
            let out = conn.out.clone();
            crate::spend::forward(&conn.user, changes, out, || {
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
    spend.abort();
    sessions.abort();
    writer.abort();
    tracing::info!(connection = %conn.id, "UI disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SessionStore;
    use crate::store::memory::MemoryStore;

    /// Records `count` updates in a session's log and gives back their sequence numbers.
    async fn record(store: &MemoryStore, session: Uuid, count: usize) -> Vec<i64> {
        let mut seqs = Vec::new();
        for n in 0..count {
            let (seq, _) = store
                .append_event(
                    session,
                    &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": n.to_string()}}),
                )
                .await
                .unwrap();
            seqs.push(seq);
        }
        seqs
    }

    #[tokio::test]
    async fn a_load_after_a_sequence_replays_only_what_came_after_it() {
        let store = Arc::new(MemoryStore::default());
        let hub = Hub::new(store.clone());
        let session = Uuid::new_v4();
        // Another session's events in between, as the log's one sequence leaves a session's own numbers with gaps.
        let other = record(&store, Uuid::new_v4(), 2).await;
        let seqs = record(&store, session, 5).await;

        let (replayed, resumed) = replay_page(&hub, session, seqs[4], Some(seqs[2]))
            .await
            .unwrap();

        assert!(resumed);
        let replayed: Vec<i64> = replayed.iter().map(|(seq, _, _)| *seq).collect();
        // This session's own newer events, and nothing of the other session's in between.
        assert_eq!(replayed, seqs[3..]);
        assert!(!replayed.contains(&other[1]));
    }

    #[tokio::test]
    async fn a_load_after_the_last_sequence_replays_nothing() {
        let store = Arc::new(MemoryStore::default());
        let hub = Hub::new(store.clone());
        let session = Uuid::new_v4();
        let seqs = record(&store, session, 3).await;

        let (replayed, resumed) = replay_page(&hub, session, seqs[2], Some(seqs[2]))
            .await
            .unwrap();

        assert!(resumed);
        assert!(replayed.is_empty());
    }

    #[tokio::test]
    async fn a_load_after_a_sequence_the_log_does_not_hold_replays_the_latest_page() {
        let store = Arc::new(MemoryStore::default());
        let hub = Hub::new(store.clone());
        let session = Uuid::new_v4();
        // Recorded before this session's own events, so the point is one the log never held for it — what a UI
        // resuming from a sequence the log has dropped says (#449).
        let dropped = record(&store, Uuid::new_v4(), 2).await;
        let seqs = record(&store, session, 4).await;

        let (replayed, resumed) = replay_page(&hub, session, seqs[3], Some(dropped[0]))
            .await
            .unwrap();

        assert!(!resumed);
        let replayed: Vec<i64> = replayed.iter().map(|(seq, _, _)| *seq).collect();
        assert_eq!(replayed, seqs);
    }

    #[tokio::test]
    async fn a_load_further_behind_than_a_page_replays_the_latest_page() {
        let store = Arc::new(MemoryStore::default());
        let hub = Hub::new(store.clone());
        let session = Uuid::new_v4();
        let seqs = record(&store, session, HISTORY_PAGE as usize + 2).await;

        let (replayed, resumed) = replay_page(&hub, session, *seqs.last().unwrap(), Some(seqs[0]))
            .await
            .unwrap();

        assert!(!resumed);
        assert_eq!(replayed.len() as i64, HISTORY_PAGE);
        assert_eq!(replayed[0].0, seqs[2]);
    }
}
