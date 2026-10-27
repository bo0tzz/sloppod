//! Live session state between the agent (over the supervisor link) and any number of UIs (over `/acp`).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Value, json};
use sloppod_protocol::api::{Phase, Proposal};
use sqlx::PgPool;
use tokio::sync::{Mutex, broadcast, oneshot};
use uuid::Uuid;

use crate::db;
use crate::link::{AgentConn, RpcError};

#[derive(Clone, Debug)]
pub enum UiEvent {
    /// A notification to forward as-is; `seq` is the event-log position for recorded updates.
    Notification { seq: Option<i64>, message: Value },
    /// A permission request or a question from the agent, for the UIs to answer.
    PermissionRequest {
        id: u64,
        method: String,
        params: Value,
    },
    PermissionResolved {
        id: u64,
        tool_call_id: Option<String>,
    },
}

/// A prompt waiting for the agent to be up or for the running turn to end; it runs with the rest of the queue
/// as the next turn.
struct Queued {
    /// Messages queued behind a running turn have an id, so UIs can show them and steer them in; they're
    /// recorded when they run.
    id: Option<Uuid>,
    blocks: Vec<Value>,
    reply: Option<oneshot::Sender<Result<Value, RpcError>>>,
}

struct Permission {
    agent_request_id: Value,
    method: String,
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
    queue: Vec<Queued>,
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
    /// Open permission requests and questions: id, method, params.
    pub permissions: Vec<(u64, String, Value)>,
    /// The `_sloppod/queue` notification for messages waiting for the running turn to end.
    pub queue: Value,
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

    /// The agent is up; anything sent while it wasn't runs now, as one turn.
    pub async fn agent_ready(
        self: &Arc<Self>,
        id: Uuid,
        agent: &Arc<AgentConn>,
        agent_session_id: String,
        agent_state: Value,
        pending: Option<String>,
    ) {
        let queued = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            if !state.agent.as_ref().is_some_and(|a| Arc::ptr_eq(a, agent)) {
                return;
            }
            state.agent_session_id = Some(agent_session_id);
            state.agent_state = agent_state;
            state.loading = false;
            let queued = std::mem::take(&mut state.queue);
            if pending.is_some() || !queued.is_empty() {
                state.turn_running = true;
            }
            queued
        };
        if pending.is_none() && queued.is_empty() {
            self.set_phase(id, Phase::Idle).await;
            return;
        }
        tokio::spawn(self.clone().run_queued(id, pending, queued));
    }

    /// Runs queued prompts (and a prompt the agent was holding, if any) as one turn, and answers each prompt
    /// with its outcome. Boxed because a turn ending starts the next queued one.
    fn run_queued(
        self: Arc<Self>,
        id: Uuid,
        pending: Option<String>,
        queued: Vec<Queued>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            for q in queued.iter().filter(|q| q.id.is_some()) {
                self.record_user_message(id, &json!(q.blocks), false).await;
            }
            let mut blocks: Vec<Value> = pending
                .into_iter()
                .map(|text| json!({"type": "text", "text": text}))
                .collect();
            blocks.extend(queued.iter().flat_map(|q| q.blocks.iter().cloned()));
            let result = self.run_turn(id, blocks).await;
            if let Err(e) = &result {
                tracing::warn!(session = %id, error = %e, "queued prompt failed");
            }
            for reply in queued.into_iter().filter_map(|q| q.reply) {
                let _ = reply.send(result.clone());
            }
        })
    }

    /// The `_sloppod/queue` notification for the messages waiting behind the running turn.
    fn queue_notification(id: Uuid, queue: &[Queued]) -> Value {
        let messages: Vec<Value> = queue
            .iter()
            .filter_map(|q| q.id.map(|qid| json!({"id": qid, "prompt": q.blocks})))
            .collect();
        notification(
            "_sloppod/queue",
            json!({"sessionId": id, "messages": messages}),
        )
    }

    async fn broadcast_queue(&self, id: Uuid) {
        let message = {
            let live = self.live(id);
            let state = live.state.lock().await;
            Self::queue_notification(id, &state.queue)
        };
        self.broadcast(id, UiEvent::Notification { seq: None, message });
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
            match db::set_auto_title(&self.db, id, title).await {
                Ok(true) => self.broadcast_session(id).await,
                Ok(false) => {}
                Err(e) => tracing::warn!(session = %id, error = %e, "setting title"),
            }
        }
        self.record(id, update).await;
    }

    /// Handles a request from the agent: permission requests and questions, which the UIs answer.
    pub async fn on_agent_request(&self, id: Uuid, agent: &Arc<AgentConn>, message: Value) {
        let request_id = message["id"].clone();
        let method = message["method"].as_str().unwrap_or_default().to_owned();
        if !matches!(
            method.as_str(),
            "session/request_permission" | "elicitation/create"
        ) {
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
                    method: method.clone(),
                    params: params.clone(),
                    agent: agent.clone(),
                },
            );
            pid
        };
        self.set_phase(id, Phase::Waiting).await;
        self.broadcast(
            id,
            UiEvent::PermissionRequest {
                id: pid,
                method,
                params,
            },
        );
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

    pub async fn record_user_message(&self, id: Uuid, prompt: &Value, steering: bool) {
        let message_id = Uuid::new_v4();
        for block in prompt.as_array().into_iter().flatten() {
            let mut update = json!({"sessionUpdate": "user_message_chunk", "messageId": message_id, "content": block});
            if steering {
                update["_meta"] = json!({"sloppod": {"steering": true}});
            }
            self.record(id, update).await;
        }
    }

    /// Starts a turn, or queues the prompt until the agent is up or the running turn ends; resolves when its
    /// turn ends.
    pub async fn prompt(self: &Arc<Self>, id: Uuid, params: Value) -> Result<Value, RpcError> {
        let blocks = params["prompt"].as_array().cloned().unwrap_or_default();
        let (queued, behind_turn) = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            let agent_up = state.agent.is_some() && state.agent_session_id.is_some();
            if agent_up && !state.turn_running {
                state.turn_running = true;
                (None, false)
            } else {
                let (reply, receiver) = oneshot::channel();
                state.queue.push(Queued {
                    id: agent_up.then(Uuid::new_v4),
                    blocks: blocks.clone(),
                    reply: Some(reply),
                });
                (Some(receiver), agent_up)
            }
        };
        if behind_turn {
            self.broadcast_queue(id).await;
        } else {
            self.record_user_message(id, &params["prompt"], false).await;
        }
        match queued {
            Some(receiver) => receiver.await.unwrap_or_else(|_| {
                Err(RpcError::new(
                    -32002,
                    "the session stopped before the prompt ran",
                ))
            }),
            None => self.run_turn(id, blocks).await,
        }
    }

    /// Runs one turn; the caller has set `turn_running`. Messages queued behind it run as the next turn.
    async fn run_turn(self: &Arc<Self>, id: Uuid, blocks: Vec<Value>) -> Result<Value, RpcError> {
        let (agent, agent_session_id) = match self.agent_for(id).await {
            Ok(agent) => agent,
            Err(e) => {
                self.live(id).state.lock().await.turn_running = false;
                return Err(e);
            }
        };
        self.set_phase(id, Phase::Working).await;
        let result = agent
            .request(
                "session/prompt",
                json!({"sessionId": agent_session_id, "prompt": blocks}),
            )
            .await;
        let next = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            let agent_up = state.agent.is_some() && state.agent_session_id.is_some();
            state.turn_running = agent_up && !state.queue.is_empty();
            state.turn_running.then(|| std::mem::take(&mut state.queue))
        };
        if self.agent_for(id).await.is_ok() {
            agent.sync_transcript();
            match next {
                Some(queued) => {
                    self.broadcast_queue(id).await;
                    tokio::spawn(self.clone().run_queued(id, None, queued));
                }
                None => self.set_phase(id, Phase::Idle).await,
            }
        }
        result
    }

    /// Moves a message waiting behind the running turn into it.
    pub async fn steer_queued(
        self: &Arc<Self>,
        id: Uuid,
        params: Value,
    ) -> Result<Value, RpcError> {
        let qid = params["id"]
            .as_str()
            .and_then(|q| q.parse::<Uuid>().ok())
            .ok_or_else(|| RpcError::new(-32602, "missing or invalid id"))?;
        let queued = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            let index = state
                .queue
                .iter()
                .position(|q| q.id == Some(qid))
                .ok_or_else(|| RpcError::new(-32002, "that message isn't queued any more"))?;
            state.queue.remove(index)
        };
        self.broadcast_queue(id).await;
        self.record_user_message(id, &json!(queued.blocks), true)
            .await;
        let result = self.steer_in(id, queued.blocks).await;
        if let Some(reply) = queued.reply {
            let _ = reply.send(result.clone());
        }
        result
    }

    /// Tells the agent something happened outside its turns, e.g. a proposal was decided: a new turn when it's
    /// idle, steered into the running one, or queued until it's up.
    pub async fn notify(self: &Arc<Self>, id: Uuid, text: String) {
        let blocks = vec![json!({"type": "text", "text": text})];
        let delivery = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            if state.agent.is_none() || state.agent_session_id.is_none() {
                state.queue.push(Queued {
                    id: None,
                    blocks: blocks.clone(),
                    reply: None,
                });
                None
            } else if state.turn_running {
                Some(false)
            } else {
                state.turn_running = true;
                Some(true)
            }
        };
        self.record(
            id,
            json!({
                "sessionUpdate": "user_message_chunk",
                "messageId": Uuid::new_v4(),
                "content": blocks[0],
                "_meta": {"sloppod": {"notice": true}},
            }),
        )
        .await;
        match delivery {
            None => {}
            Some(true) => {
                let hub = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = hub.run_turn(id, blocks).await {
                        tracing::warn!(session = %id, error = %e, "notice turn failed");
                    }
                });
            }
            Some(false) => {
                if let Err(e) = self.steer_in(id, blocks).await {
                    tracing::warn!(session = %id, error = %e, "steering a notice in");
                }
            }
        }
    }

    /// Steers blocks into the running turn. If that turn has just ended they start a new one here, so the turn
    /// is tracked like any other rather than run detached by the adapter.
    async fn steer_in(self: &Arc<Self>, id: Uuid, blocks: Vec<Value>) -> Result<Value, RpcError> {
        for _ in 0..10 {
            let (agent, agent_session_id) = self.agent_for(id).await?;
            let result = agent
                .request(
                    "_session/steering",
                    json!({
                        "sessionId": agent_session_id,
                        "prompt": blocks,
                        "_meta": {"steering": {"idleBehavior": "promptRequired"}},
                    }),
                )
                .await?;
            if result["outcome"] != "promptRequired" {
                return Ok(result);
            }
            let start = {
                let live = self.live(id);
                let mut state = live.state.lock().await;
                !std::mem::replace(&mut state.turn_running, true)
            };
            if start {
                let hub = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = hub.run_turn(id, blocks).await {
                        tracing::warn!(session = %id, error = %e, "steered turn failed");
                    }
                });
                return Ok(json!({"outcome": "startedNewTurn"}));
            }
            // Our record of the turn hasn't caught up with the agent's yet.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Err(RpcError::new(
            -32002,
            "couldn't deliver the message; send it again",
        ))
    }

    /// Delivers a message into the running turn; before the agent is up it joins the queue instead.
    pub async fn steer(self: &Arc<Self>, id: Uuid, params: Value) -> Result<Value, RpcError> {
        let blocks = params["prompt"].as_array().cloned().unwrap_or_default();
        if self.agent_for(id).await.is_err() {
            self.live(id).state.lock().await.queue.push(Queued {
                id: None,
                blocks,
                reply: None,
            });
            self.record_user_message(id, &params["prompt"], false).await;
            return Ok(json!({"outcome": "queued"}));
        }
        self.record_user_message(id, &params["prompt"], true).await;
        self.steer_in(id, blocks).await
    }

    pub async fn cancel(&self, id: Uuid) {
        if let Ok((agent, agent_session_id)) = self.agent_for(id).await {
            agent.notify("session/cancel", json!({"sessionId": agent_session_id}));
        }
        // ACP: a client that cancels a turn answers its outstanding permission requests and questions as cancelled.
        let pending: Vec<(u64, bool)> = self
            .live(id)
            .state
            .lock()
            .await
            .permissions
            .iter()
            .map(|(pid, p)| (*pid, p.method == "elicitation/create"))
            .collect();
        for (pid, question) in pending {
            let answer = if question {
                json!({"action": "cancel"})
            } else {
                json!({"outcome": {"outcome": "cancelled"}})
            };
            self.answer_permission(id, pid, Ok(answer)).await;
        }
    }

    /// Changes a session config option; the mode is fixed, and a model change sticks across restarts.
    pub async fn set_config_option(&self, id: Uuid, mut params: Value) -> Result<Value, RpcError> {
        if params["configId"] == "mode" {
            return Err(RpcError::new(-32602, "sessions always run in auto mode"));
        }
        let (agent, agent_session_id) = self.agent_for(id).await?;
        params["sessionId"] = json!(agent_session_id);
        let result = agent
            .request("session/set_config_option", params.clone())
            .await?;
        if let Some(options) = result.get("configOptions") {
            self.live(id).state.lock().await.agent_state["configOptions"] = options.clone();
        }
        if params["configId"] == "model"
            && let Some(model) = params["value"].as_str()
            && let Err(e) = db::set_session_model(&self.db, id, model).await
        {
            tracing::warn!(session = %id, error = %e, "recording the session's model");
        }
        Ok(result)
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
        let mut permissions: Vec<(u64, String, Value)> = state
            .permissions
            .iter()
            .map(|(pid, p)| (*pid, p.method.clone(), p.params.clone()))
            .collect();
        permissions.sort_by_key(|(pid, _, _)| *pid);
        Ok(Replay {
            events,
            agent_state,
            permissions,
            queue: Self::queue_notification(id, &state.queue),
        })
    }

    pub async fn stop(&self, id: Uuid) {
        let (agent, queued) = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            (state.agent.take(), std::mem::take(&mut state.queue))
        };
        for reply in queued.into_iter().filter_map(|q| q.reply) {
            let _ = reply.send(Err(RpcError::new(-32002, "the session was stopped")));
        }
        if let Some(agent) = agent {
            agent.close();
        }
        self.set_phase(id, Phase::Stopped).await;
    }
}

fn tool_call_id(params: &Value) -> Option<String> {
    params["toolCall"]["toolCallId"]
        .as_str()
        .or(params["toolCallId"].as_str())
        .map(str::to_owned)
}
