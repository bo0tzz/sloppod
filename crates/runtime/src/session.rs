//! Live session state between the agent (over the supervisor link) and any number of UIs (over `/acp`).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{Value, json};
use sloppod_protocol::api::{Phase, Proposal};
use sloppod_protocol::link::RunningAgent;
use tokio::sync::{Mutex, broadcast, oneshot};
use uuid::Uuid;

use crate::link::{AgentConn, RpcError};
use crate::store::SessionStore;

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
pub(crate) struct Queued {
    /// Messages queued behind a running turn have an id, so UIs can show them and steer them in; they're
    /// recorded when they run.
    pub(crate) id: Option<Uuid>,
    pub(crate) blocks: Vec<Value>,
    pub(crate) reply: Option<oneshot::Sender<Result<Value, RpcError>>>,
}

struct Permission {
    method: String,
    params: Value,
    asker: Asker,
}

/// Who waits on a permission request or question.
enum Asker {
    /// The agent, blocked on its request until it's answered.
    Agent {
        request_id: Value,
        agent: Arc<AgentConn>,
    },
    /// The agent's `ask_user` tool, which returned at once: the answer goes to the agent as a notice.
    Tool { questions: Value },
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
    /// For an agent a runtime restart cut off, if it has to be launched again rather than attached to.
    restart_notice: Option<String>,
    /// The session was stopped: nothing queued for it runs any more.
    stopped: bool,
}

pub struct Live {
    pub id: Uuid,
    events: broadcast::Sender<UiEvent>,
    state: Mutex<LiveState>,
    /// Held while the session is parked or woken, so the two never overlap.
    transition: Mutex<()>,
}

/// What `run_queued` does with its batch.
enum Start {
    /// Runs it on the agent it was taken for, in this agent session.
    Run(String, Vec<Queued>),
    /// The session was stopped: its callers get an error.
    Stopped(Vec<Queued>),
    /// Back in the queue, and run on a newer agent that was up and idle.
    Requeued(Option<(Arc<AgentConn>, Vec<Queued>)>),
}

/// Why a session couldn't be parked.
#[derive(Debug, thiserror::Error)]
pub enum ParkError {
    #[error("the session is in a turn; stop it or let it finish before parking")]
    InTurn,
    #[error("the session is deleted")]
    Stopped,
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// Brings a parked session back; the runtime's is `lifecycle::wake`, which leaves other sessions alone.
pub type Waker =
    Arc<dyn Fn(Uuid) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;

pub struct Hub {
    store: Arc<dyn SessionStore>,
    waker: StdMutex<Option<Waker>>,
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
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            waker: StdMutex::default(),
            sessions: StdMutex::default(),
            agent_capabilities: StdMutex::default(),
        }
    }

    pub fn set_waker(&self, waker: Waker) {
        *self.waker.lock().unwrap() = Some(waker);
    }

    /// Wakes the session if it's parked, after any park in progress, so a message queued for an absent agent
    /// can't be left waiting on a parked session.
    async fn wake_for_message(&self, id: Uuid) -> Result<(), RpcError> {
        let waker = self.waker.lock().unwrap().clone();
        match waker {
            Some(waker) => waker(id).await.map_err(|e| {
                RpcError::new(
                    -32603,
                    format!("couldn't wake the session: {e}; the message stays queued"),
                )
            }),
            None => Ok(()),
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
                    transition: Mutex::default(),
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
        match self.store.set_phase(id, phase).await {
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
        if let Ok(Some(row)) = self.store.get_session(id).await {
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
        match self.store.append_event(id, &update).await {
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

    pub async fn set_restart_notice(&self, id: Uuid, notice: String) {
        self.live(id).state.lock().await.restart_notice = Some(notice);
    }

    pub async fn take_restart_notice(&self, id: Uuid) -> Option<String> {
        self.live(id).state.lock().await.restart_notice.take()
    }

    /// Carries on with an agent that outlived the last link: its session, the turn it's running and the questions
    /// it's waiting on.
    pub async fn adopt_agent(
        self: &Arc<Self>,
        id: Uuid,
        agent: &Arc<AgentConn>,
        running: RunningAgent,
    ) {
        if self.agent_capabilities().is_none() {
            self.set_agent_capabilities(running.ready.capabilities);
        }
        let mut turn = None;
        for request in &running.open {
            if let Some(request_id) = request["id"].as_i64() {
                let reply = agent.adopt(request_id);
                if request["method"] == "session/prompt" {
                    turn = Some(reply);
                }
            }
        }
        let agent_state = match self.store.get_session(id).await {
            Ok(row) => row.and_then(|r| r.agent_state),
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "loading the agent's state");
                None
            }
        }
        .unwrap_or_else(|| json!({}));
        match turn {
            None => {
                self.agent_ready(id, agent, running.ready.agent_session_id, agent_state, None)
                    .await
            }
            Some(turn) => {
                {
                    let live = self.live(id);
                    let mut state = live.state.lock().await;
                    if !state.agent.as_ref().is_some_and(|a| Arc::ptr_eq(a, agent)) {
                        return;
                    }
                    state.agent_session_id = Some(running.ready.agent_session_id);
                    state.agent_state = agent_state;
                    state.loading = false;
                    state.turn_running = true;
                    self.set_phase(id, Phase::Working).await;
                }
                let (hub, agent) = (self.clone(), agent.clone());
                tokio::spawn(async move {
                    if let Ok(Err(e)) = turn.await {
                        tracing::warn!(session = %id, error = %e, "adopted turn failed");
                    }
                    hub.end_turn(id, &agent).await;
                });
            }
        }
        for request in running.waiting {
            self.on_agent_request(id, agent, request).await;
        }
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
            } else {
                self.set_phase(id, Phase::Idle).await;
            }
            queued
        };
        if pending.is_none() && queued.is_empty() {
            return;
        }
        tokio::spawn(self.clone().run_queued(id, agent.clone(), pending, queued));
    }

    /// Runs queued prompts (and a prompt the agent was holding, if any) as one turn, and answers each prompt
    /// with its outcome. Boxed because a turn ending starts the next queued one.
    pub(crate) fn run_queued(
        self: Arc<Self>,
        id: Uuid,
        taken_for: Arc<AgentConn>,
        pending: Option<String>,
        queued: Vec<Queued>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            for q in queued.iter().filter(|q| q.id.is_some()) {
                self.record_user_message(id, &json!(q.blocks), false, q.id)
                    .await;
            }
            let mut blocks: Vec<Value> = pending
                .iter()
                .map(|text| json!({"type": "text", "text": text}))
                .collect();
            blocks.extend(queued.iter().flat_map(|q| q.blocks.iter().cloned()));
            // One decision under the lock, so no other agent's readiness or turn can slip in between.
            let started = {
                let live = self.live(id);
                let mut state = live.state.lock().await;
                let current = state
                    .agent
                    .as_ref()
                    .is_some_and(|a| Arc::ptr_eq(a, &taken_for));
                match (&state.agent_session_id, current) {
                    (Some(agent_session_id), true) => {
                        let agent_session_id = agent_session_id.clone();
                        // The first message is kept until now, so an agent that went before it ran leaves it for
                        // the next; from here this turn has it.
                        if pending.is_some()
                            && let Err(e) = self.store.clear_pending_prompt(id).await
                        {
                            tracing::warn!(session = %id, error = %e, "clearing the first message");
                        }
                        self.set_phase(id, Phase::Working).await;
                        Start::Run(agent_session_id, queued)
                    }
                    _ if state.stopped => Start::Stopped(queued),
                    _ => {
                        // Taken for an agent that's gone: everything goes back, ahead of what's queued since,
                        // recorded already so not shown as waiting behind a turn. A newer agent that's up and idle
                        // runs it now. The first message isn't requeued: it's still stored, and the next agent to
                        // be ready reads it from there.
                        let requeued = queued.into_iter().map(|q| Queued { id: None, ..q });
                        let rest = std::mem::take(&mut state.queue);
                        state.queue = requeued.chain(rest).collect();
                        let idle = state.agent_session_id.is_some()
                            && !state.turn_running
                            && !state.queue.is_empty();
                        match (state.agent.clone(), idle) {
                            (Some(agent), true) => {
                                state.turn_running = true;
                                Start::Requeued(Some((agent, std::mem::take(&mut state.queue))))
                            }
                            _ => Start::Requeued(None),
                        }
                    }
                }
            };
            let (agent_session_id, queued) = match started {
                Start::Run(agent_session_id, queued) => (agent_session_id, queued),
                Start::Stopped(queued) => {
                    for reply in queued.into_iter().filter_map(|q| q.reply) {
                        let _ = reply.send(Err(RpcError::new(-32002, "the session was stopped")));
                    }
                    return;
                }
                Start::Requeued(next) => {
                    if let Some((agent, queued)) = next {
                        tokio::spawn(self.clone().run_queued(id, agent, None, queued));
                    }
                    return;
                }
            };
            let agent = taken_for;
            let result = self.turn_on(id, &agent, &agent_session_id, blocks).await;
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
            self.set_phase(id, Phase::Disconnected).await;
            state
                .permissions
                .extract_if(|_, p| matches!(p.asker, Asker::Agent { .. }))
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
            match self.store.set_auto_title(id, title).await {
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
            if !state.agent.as_ref().is_some_and(|a| Arc::ptr_eq(a, agent)) {
                agent.respond(
                    request_id,
                    Err(RpcError::new(-32002, "this agent connection was replaced")),
                );
                return;
            }
            state.next_permission += 1;
            let pid = state.next_permission;
            state.permissions.insert(
                pid,
                Permission {
                    method: method.clone(),
                    params: params.clone(),
                    asker: Asker::Agent {
                        request_id,
                        agent: agent.clone(),
                    },
                },
            );
            self.set_phase(id, Phase::Waiting).await;
            pid
        };
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
    pub async fn answer_permission(
        self: &Arc<Self>,
        id: Uuid,
        pid: u64,
        result: Result<Value, RpcError>,
    ) {
        let permission = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            let permission = state.permissions.remove(&pid);
            let blocked = |p: &Permission| matches!(p.asker, Asker::Agent { .. });
            if permission.as_ref().is_some_and(blocked)
                && !state.permissions.values().any(blocked)
                && state.agent.is_some()
            {
                let phase = if state.turn_running {
                    Phase::Working
                } else {
                    Phase::Idle
                };
                self.set_phase(id, phase).await;
            }
            permission
        };
        let Some(permission) = permission else { return };
        let tool_call_id = tool_call_id(&permission.params);
        match permission.asker {
            Asker::Agent { request_id, agent } => agent.respond(request_id, result),
            Asker::Tool { questions } => {
                let answer = result.unwrap_or_else(|_| json!({"action": "cancel"}));
                self.notify(id, crate::questions::answer_text(&questions, &answer))
                    .await;
            }
        }
        self.broadcast(
            id,
            UiEvent::PermissionResolved {
                id: pid,
                tool_call_id,
            },
        );
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

    /// Records what the user sent, under the id their UI gave it if any, so a UI that lost its connection can
    /// tell whether it arrived.
    pub async fn record_user_message(
        &self,
        id: Uuid,
        prompt: &Value,
        steering: bool,
        message_id: Option<Uuid>,
    ) {
        let message_id = message_id.unwrap_or_else(Uuid::new_v4);
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
        let message_id = client_message_id(&params);
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
                    id: agent_up.then(|| message_id.unwrap_or_else(Uuid::new_v4)),
                    blocks: blocks.clone(),
                    reply: Some(reply),
                });
                (Some(receiver), agent_up)
            }
        };
        if behind_turn {
            self.broadcast_queue(id).await;
        } else {
            self.record_user_message(id, &params["prompt"], false, message_id)
                .await;
        }
        if queued.is_some() && !behind_turn {
            self.wake_for_message(id).await?;
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
        let Some((agent, agent_session_id)) = self.begin_turn(id).await else {
            self.live(id).state.lock().await.turn_running = false;
            return Err(RpcError::new(
                -32002,
                "the session's agent isn't running yet; try again shortly",
            ));
        };
        self.turn_on(id, &agent, &agent_session_id, blocks).await
    }

    /// Marks a turn working on the session's current agent, which it returns; `None` if there isn't one up.
    /// The turn and agent paths (starting and ending turns, an agent getting ready, adopted or detached, permission
    /// requests and answers) write phase under the state lock, so their writes follow the state they're derived
    /// from rather than racing each other. Park and wake write it under the session's transition lock instead,
    /// and stop writes it last, once nothing can run any more.
    async fn begin_turn(&self, id: Uuid) -> Option<(Arc<AgentConn>, String)> {
        let live = self.live(id);
        let state = live.state.lock().await;
        let (Some(agent), Some(agent_session_id)) = (&state.agent, &state.agent_session_id) else {
            return None;
        };
        let started = (agent.clone(), agent_session_id.clone());
        self.set_phase(id, Phase::Working).await;
        Some(started)
    }

    async fn turn_on(
        self: &Arc<Self>,
        id: Uuid,
        agent: &Arc<AgentConn>,
        agent_session_id: &str,
        blocks: Vec<Value>,
    ) -> Result<Value, RpcError> {
        let result = agent
            .request(
                "session/prompt",
                json!({"sessionId": agent_session_id, "prompt": blocks}),
            )
            .await;
        self.end_turn(id, agent).await;
        result
    }

    /// Syncs the transcript after a turn and runs the messages queued behind it as the next one. A turn that ran
    /// on an agent since replaced leaves the session to the new one.
    pub(crate) async fn end_turn(self: &Arc<Self>, id: Uuid, agent: &Arc<AgentConn>) {
        let next = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            if !state.agent.as_ref().is_some_and(|a| Arc::ptr_eq(a, agent)) {
                return;
            }
            let agent_up = state.agent_session_id.is_some();
            state.turn_running = agent_up && !state.queue.is_empty();
            let next = state.turn_running.then(|| std::mem::take(&mut state.queue));
            if next.is_none() && agent_up {
                self.set_phase(id, Phase::Idle).await;
            }
            next
        };
        agent.sync_transcript();
        if let Some(queued) = next {
            self.broadcast_queue(id).await;
            tokio::spawn(self.clone().run_queued(id, agent.clone(), None, queued));
        }
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
        self.record_user_message(id, &json!(queued.blocks), true, queued.id)
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
        let message_id = client_message_id(&params);
        if self.agent_for(id).await.is_err() {
            self.live(id).state.lock().await.queue.push(Queued {
                id: None,
                blocks,
                reply: None,
            });
            self.record_user_message(id, &params["prompt"], false, message_id)
                .await;
            self.wake_for_message(id).await?;
            return Ok(json!({"outcome": "queued"}));
        }
        self.record_user_message(id, &params["prompt"], true, message_id)
            .await;
        self.steer_in(id, blocks).await
    }

    pub async fn cancel(self: &Arc<Self>, id: Uuid) {
        if let Ok((agent, agent_session_id)) = self.agent_for(id).await {
            agent.notify("session/cancel", json!({"sessionId": agent_session_id}));
        }
        // ACP: a client that cancels a turn answers its outstanding permission requests and questions as cancelled.
        // Questions from `ask_user` don't belong to the turn, so they stay.
        let pending: Vec<(u64, bool)> = self
            .live(id)
            .state
            .lock()
            .await
            .permissions
            .iter()
            .filter(|(_, p)| matches!(p.asker, Asker::Agent { .. }))
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
            && let Err(e) = self.store.set_session_model(id, model).await
        {
            tracing::warn!(session = %id, error = %e, "recording the session's model");
        }
        Ok(result)
    }

    /// Everything a UI needs when it loads a session.
    pub async fn replay(&self, id: Uuid) -> Result<Replay, sqlx::Error> {
        let events = self.store.events(id).await?;
        let live = self.live(id);
        let state = live.state.lock().await;
        let agent_state = if state.agent_state.is_null() {
            self.store
                .get_session(id)
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

    /// Shows the user questions from the agent's `ask_user` tool, without the agent waiting: the answer reaches it
    /// as a notice.
    pub async fn ask(&self, id: Uuid, questions: Value) -> Result<(), String> {
        let params = crate::questions::elicitation(&questions, &id.to_string())?;
        let pid = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            state.next_permission += 1;
            let pid = state.next_permission;
            state.permissions.insert(
                pid,
                Permission {
                    method: "elicitation/create".into(),
                    params: params.clone(),
                    asker: Asker::Tool { questions },
                },
            );
            pid
        };
        self.broadcast(
            id,
            UiEvent::PermissionRequest {
                id: pid,
                method: "elicitation/create".into(),
                params,
            },
        );
        Ok(())
    }

    pub async fn turn_running(&self, id: Uuid) -> bool {
        self.live(id).state.lock().await.turn_running
    }

    /// Parks a session between turns (ADR 5): lets go of its agent, then `suspend` ends its pod. Messages sent
    /// meanwhile stay queued for its wake. Refused during a turn, as one that starts while parking can't.
    pub async fn park<F, Fut>(self: &Arc<Self>, id: Uuid, suspend: F) -> Result<(), ParkError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<()>>,
    {
        let live = self.live(id);
        let _transition = live.transition.lock().await;
        match self
            .store
            .get_session(id)
            .await
            .map_err(anyhow::Error::from)?
        {
            Some(row) if row.phase() == Phase::Stopped => return Err(ParkError::Stopped),
            Some(row) if row.phase() == Phase::Parked => return Ok(()),
            Some(_) => {}
            None => return Err(ParkError::Stopped),
        }
        let (agent, agent_session_id) = {
            let mut state = live.state.lock().await;
            if state.turn_running {
                return Err(ParkError::InTurn);
            }
            (state.agent.take(), state.agent_session_id.take())
        };
        // Parked before the pod ends, so its supervisor's attempts to reconnect meanwhile are turned away.
        self.set_phase(id, Phase::Parked).await;
        if let Err(e) = suspend().await {
            self.unpark(id, agent, agent_session_id).await;
            return Err(e.into());
        }
        if let Some(agent) = agent {
            agent.close();
        }
        Ok(())
    }

    /// Gives a session whose suspend failed its agent back, and runs what was sent to it meanwhile.
    async fn unpark(
        self: &Arc<Self>,
        id: Uuid,
        agent: Option<Arc<AgentConn>>,
        agent_session_id: Option<String>,
    ) {
        let live = self.live(id);
        let mut state = live.state.lock().await;
        let up = agent.is_some() && agent_session_id.is_some();
        state.agent = agent.clone();
        state.agent_session_id = agent_session_id;
        match agent.filter(|_| up) {
            Some(agent) if !state.queue.is_empty() => {
                let queued = std::mem::take(&mut state.queue);
                state.turn_running = true;
                self.set_phase(id, Phase::Working).await;
                tokio::spawn(self.clone().run_queued(id, agent, None, queued));
            }
            Some(_) => self.set_phase(id, Phase::Idle).await,
            None => self.set_phase(id, Phase::Disconnected).await,
        }
    }

    /// Wakes a parked session: `place` brings its pod back. A failure leaves it parked, so waking can be retried.
    /// Sessions that aren't parked are left as they are.
    pub async fn wake<F, Fut>(&self, id: Uuid, place: F) -> anyhow::Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<()>>,
    {
        let live = self.live(id);
        let _transition = live.transition.lock().await;
        let parked = self
            .store
            .get_session(id)
            .await?
            .is_some_and(|row| row.phase() == Phase::Parked);
        if !parked {
            return Ok(());
        }
        self.set_phase(id, Phase::Starting).await;
        if let Err(e) = place().await {
            self.set_phase(id, Phase::Parked).await;
            return Err(e);
        }
        Ok(())
    }

    pub async fn stop(&self, id: Uuid) {
        let (agent, queued) = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            state.stopped = true;
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

/// The id the UI gave a message it sent (`_meta.sloppod.messageId`), if it gave one.
fn client_message_id(params: &Value) -> Option<Uuid> {
    params["_meta"]["sloppod"]["messageId"]
        .as_str()
        .and_then(|s| s.parse().ok())
}

fn tool_call_id(params: &Value) -> Option<String> {
    params["toolCall"]["toolCallId"]
        .as_str()
        .or(params["toolCallId"].as_str())
        .map(str::to_owned)
}
