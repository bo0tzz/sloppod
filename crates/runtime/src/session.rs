//! Live session state between the agent (over the supervisor link) and any number of UIs (over `/acp`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Value, json};
use sloppod_protocol::api::{Phase, Proposal};
use sqlx::PgPool;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

use crate::db;
use crate::link::{AgentConn, RpcError};

#[derive(Clone, Debug)]
pub enum UiEvent {
    /// A notification to forward as-is; `seq` is the event-log position for recorded updates.
    Notification {
        seq: Option<i64>,
        message: Value,
    },
    PermissionRequest {
        id: u64,
        params: Value,
    },
    PermissionResolved {
        id: u64,
        tool_call_id: Option<String>,
    },
}

struct Permission {
    agent_request_id: Value,
    params: Value,
    agent: Arc<AgentConn>,
}

#[derive(Default)]
struct LiveState {
    agent: Option<Arc<AgentConn>>,
    agent_session_id: Option<String>,
    agent_state: Value,
    /// The agent is replaying history for `session/load`; that replay isn't new.
    loading: bool,
    turn_running: bool,
    permissions: HashMap<u64, Permission>,
    next_permission: u64,
}

pub struct Live {
    pub id: Uuid,
    events: broadcast::Sender<UiEvent>,
    state: Mutex<LiveState>,
}

pub struct Hub {
    db: PgPool,
    sessions: StdMutex<HashMap<Uuid, Arc<Live>>>,
    agent_capabilities: StdMutex<Option<Value>>,
}

fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub fn session_update(session: Uuid, update: &Value) -> Value {
    notification(
        "session/update",
        json!({"sessionId": session, "update": update}),
    )
}

pub struct Replay {
    pub events: Vec<(i64, Value)>,
    pub agent_state: Value,
    pub permissions: Vec<(u64, Value)>,
}

impl Hub {
    pub fn new(db: PgPool) -> Self {
        Self {
            db,
            sessions: StdMutex::default(),
            agent_capabilities: StdMutex::default(),
        }
    }

    pub fn live(&self, id: Uuid) -> Arc<Live> {
        let mut sessions = self.sessions.lock().expect("hub lock");
        sessions
            .entry(id)
            .or_insert_with(|| {
                Arc::new(Live {
                    id,
                    events: broadcast::channel(4096).0,
                    state: Mutex::default(),
                })
            })
            .clone()
    }

    pub fn subscribe(&self, id: Uuid) -> broadcast::Receiver<UiEvent> {
        self.live(id).events.subscribe()
    }

    pub fn agent_capabilities(&self) -> Option<Value> {
        self.agent_capabilities.lock().expect("hub lock").clone()
    }

    pub fn set_agent_capabilities(&self, capabilities: Value) {
        *self.agent_capabilities.lock().expect("hub lock") = Some(capabilities);
    }

    fn broadcast(&self, id: Uuid, event: UiEvent) {
        let _ = self.live(id).events.send(event);
    }

    pub async fn set_phase(&self, id: Uuid, phase: Phase) {
        match db::set_phase(&self.db, id, phase).await {
            Ok(Some(row)) => self.broadcast(
                id,
                UiEvent::Notification {
                    seq: None,
                    message: notification("_sloppod/session", json!({"session": row.to_api()})),
                },
            ),
            Ok(None) => {}
            Err(e) => tracing::error!(session = %id, error = %e, "setting phase"),
        }
    }

    pub async fn broadcast_session(&self, id: Uuid) {
        if let Ok(Some(row)) = db::get_session(&self.db, id).await {
            self.broadcast(
                id,
                UiEvent::Notification {
                    seq: None,
                    message: notification("_sloppod/session", json!({"session": row.to_api()})),
                },
            );
        }
    }

    pub fn broadcast_proposal(&self, proposal: &Proposal) {
        self.broadcast(
            proposal.session_id,
            UiEvent::Notification {
                seq: None,
                message: notification("_sloppod/proposal", json!({"proposal": proposal})),
            },
        );
    }

    /// Appends an update to the event log and sends it to every UI.
    pub async fn record(&self, id: Uuid, update: Value) {
        match db::append_event(&self.db, id, &update).await {
            Ok(seq) => self.broadcast(
                id,
                UiEvent::Notification {
                    seq: Some(seq),
                    message: session_update(id, &update),
                },
            ),
            Err(e) => tracing::error!(session = %id, error = %e, "recording event"),
        }
    }

    pub async fn attach_agent(&self, id: Uuid, agent: Arc<AgentConn>) {
        let live = self.live(id);
        let mut state = live.state.lock().await;
        if let Some(old) = state.agent.replace(agent) {
            old.close();
        }
        state.agent_session_id = None;
        state.turn_running = false;
    }

    pub async fn agent_ready(
        &self,
        id: Uuid,
        agent: &Arc<AgentConn>,
        agent_session_id: String,
        agent_state: Value,
    ) {
        {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            if !state.agent.as_ref().is_some_and(|a| Arc::ptr_eq(a, agent)) {
                return;
            }
            state.agent_session_id = Some(agent_session_id);
            state.agent_state = agent_state;
            state.loading = false;
        }
        self.set_phase(id, Phase::Idle).await;
    }

    pub async fn set_loading(&self, id: Uuid, loading: bool) {
        self.live(id).state.lock().await.loading = loading;
    }

    pub async fn detach_agent(&self, id: Uuid, agent: &Arc<AgentConn>) {
        let resolved: Vec<(u64, Option<String>)> = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            if !state.agent.as_ref().is_some_and(|a| Arc::ptr_eq(a, agent)) {
                return;
            }
            state.agent = None;
            state.agent_session_id = None;
            state.turn_running = false;
            state
                .permissions
                .drain()
                .map(|(pid, p)| (pid, tool_call_id(&p.params)))
                .collect()
        };
        for (pid, tool_call_id) in resolved {
            self.broadcast(
                id,
                UiEvent::PermissionResolved {
                    id: pid,
                    tool_call_id,
                },
            );
        }
        self.set_phase(id, Phase::Disconnected).await;
    }

    pub async fn on_agent_notification(&self, id: Uuid, message: Value) {
        if message["method"] != "session/update" {
            tracing::debug!(session = %id, method = %message["method"], "ignoring agent notification");
            return;
        }
        let live = self.live(id);
        let update = message["params"]["update"].clone();
        {
            let mut state = live.state.lock().await;
            if state.loading {
                return;
            }
            if update["sessionUpdate"] == "current_mode_update"
                && let Some(mode) = update.get("currentModeId")
            {
                state.agent_state["modes"]["currentModeId"] = mode.clone();
            }
        }
        if update["sessionUpdate"] == "session_info_update"
            && let Some(title) = update["title"].as_str()
        {
            if let Err(e) = db::set_title(&self.db, id, title).await {
                tracing::warn!(session = %id, error = %e, "setting title");
            }
            self.broadcast_session(id).await;
        }
        self.record(id, update).await;
    }

    /// Handles a request from the agent; only permission requests are expected.
    pub async fn on_agent_request(&self, id: Uuid, agent: &Arc<AgentConn>, message: Value) {
        let request_id = message["id"].clone();
        if message["method"] != "session/request_permission" {
            agent.respond(
                request_id,
                Err(RpcError::new(
                    -32601,
                    format!("{} is not supported by sloppod", message["method"]),
                )),
            );
            return;
        }
        let mut params = message["params"].clone();
        params["sessionId"] = json!(id);
        let pid = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            state.next_permission += 1;
            let pid = state.next_permission;
            state.permissions.insert(
                pid,
                Permission {
                    agent_request_id: request_id,
                    params: params.clone(),
                    agent: agent.clone(),
                },
            );
            pid
        };
        self.set_phase(id, Phase::Waiting).await;
        self.broadcast(id, UiEvent::PermissionRequest { id: pid, params });
    }

    /// The first answer to a permission request wins; later ones are ignored.
    pub async fn answer_permission(&self, id: Uuid, pid: u64, result: Result<Value, RpcError>) {
        let (permission, still_waiting, turn_running) = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            let permission = state.permissions.remove(&pid);
            (
                permission,
                !state.permissions.is_empty(),
                state.turn_running,
            )
        };
        let Some(permission) = permission else { return };
        let tool_call_id = tool_call_id(&permission.params);
        permission
            .agent
            .respond(permission.agent_request_id, result);
        self.broadcast(
            id,
            UiEvent::PermissionResolved {
                id: pid,
                tool_call_id,
            },
        );
        if !still_waiting {
            self.set_phase(
                id,
                if turn_running {
                    Phase::Working
                } else {
                    Phase::Idle
                },
            )
            .await;
        }
    }

    async fn agent_for(&self, id: Uuid) -> Result<(Arc<AgentConn>, String), RpcError> {
        let live = self.live(id);
        let state = live.state.lock().await;
        match (&state.agent, &state.agent_session_id) {
            (Some(agent), Some(sid)) => Ok((agent.clone(), sid.clone())),
            _ => Err(RpcError::new(
                -32002,
                "the session's agent isn't running yet; try again shortly",
            )),
        }
    }

    async fn record_user_message(&self, id: Uuid, prompt: &Value, steering: bool) {
        for block in prompt.as_array().into_iter().flatten() {
            let mut update = json!({"sessionUpdate": "user_message_chunk", "content": block});
            if steering {
                update["_meta"] = json!({"sloppod": {"steering": true}});
            }
            self.record(id, update).await;
        }
    }

    pub async fn prompt(&self, id: Uuid, mut params: Value) -> Result<Value, RpcError> {
        let (agent, agent_session_id) = self.agent_for(id).await?;
        {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            if state.turn_running {
                return Err(RpcError::new(
                    -32002,
                    "a turn is already running; steer it instead",
                ));
            }
            state.turn_running = true;
        }
        self.record_user_message(id, &params["prompt"], false).await;
        self.set_phase(id, Phase::Working).await;
        params["sessionId"] = json!(agent_session_id);
        let result = agent.request("session/prompt", params).await;
        {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            state.turn_running = false;
        }
        if self.agent_for(id).await.is_ok() {
            self.set_phase(id, Phase::Idle).await;
            agent.sync_transcript();
        }
        result
    }

    pub async fn steer(&self, id: Uuid, mut params: Value) -> Result<Value, RpcError> {
        let (agent, agent_session_id) = self.agent_for(id).await?;
        self.record_user_message(id, &params["prompt"], true).await;
        params["sessionId"] = json!(agent_session_id);
        agent.request("_session/steering", params).await
    }

    pub async fn cancel(&self, id: Uuid) {
        if let Ok((agent, agent_session_id)) = self.agent_for(id).await {
            agent.notify("session/cancel", json!({"sessionId": agent_session_id}));
        }
    }

    pub async fn set_mode(&self, id: Uuid, mut params: Value) -> Result<Value, RpcError> {
        let (agent, agent_session_id) = self.agent_for(id).await?;
        params["sessionId"] = json!(agent_session_id);
        agent.request("session/set_mode", params).await
    }

    /// Everything a UI needs when it loads a session.
    pub async fn replay(&self, id: Uuid) -> Result<Replay, sqlx::Error> {
        let events = db::events(&self.db, id).await?;
        let live = self.live(id);
        let state = live.state.lock().await;
        let agent_state = if state.agent_state.is_null() {
            db::get_session(&self.db, id)
                .await?
                .and_then(|r| r.agent_state)
                .unwrap_or_else(|| json!({}))
        } else {
            state.agent_state.clone()
        };
        let mut permissions: Vec<(u64, Value)> = state
            .permissions
            .iter()
            .map(|(pid, p)| (*pid, p.params.clone()))
            .collect();
        permissions.sort_by_key(|(pid, _)| *pid);
        Ok(Replay {
            events,
            agent_state,
            permissions,
        })
    }

    pub async fn stop(&self, id: Uuid) {
        let agent = self.live(id).state.lock().await.agent.take();
        if let Some(agent) = agent {
            agent.close();
        }
        self.set_phase(id, Phase::Stopped).await;
    }
}

fn tool_call_id(params: &Value) -> Option<String> {
    params["toolCall"]["toolCallId"].as_str().map(str::to_owned)
}
