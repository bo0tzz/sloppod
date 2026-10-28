//! Live session state between the agent (over the supervisor link) and any number of UIs (over `/acp`).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sloppod_protocol::api::{Phase, Proposal};
use sloppod_protocol::link::RunningAgent;
use tokio::sync::{Mutex, Notify, broadcast, oneshot};
use uuid::Uuid;

use crate::limits;
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
    pub(crate) notice: bool,
    pub(crate) reply: Option<oneshot::Sender<Result<Value, RpcError>>>,
}

/// How a queued prompt is kept in the store.
fn stored(q: &Queued) -> Value {
    json!({"id": q.id, "blocks": q.blocks, "notice": q.notice})
}

/// A queued prompt from how it's kept in the store.
fn unstored(q: &Value) -> Queued {
    Queued {
        id: q["id"].as_str().and_then(|s| s.parse().ok()),
        blocks: q["blocks"].as_array().cloned().unwrap_or_default(),
        notice: q["notice"] == true,
        reply: None,
    }
}

/// A notice, as a prompt of its own.
fn notice_queued(text: &str) -> Queued {
    Queued {
        id: None,
        blocks: vec![json!({"type": "text", "text": text})],
        notice: true,
        reply: None,
    }
}

/// How long the agent has to have been quiet before a prompt tells it the time.
const TIME_AFTER: chrono::Duration = chrono::Duration::minutes(30);

/// The line telling the agent the time (#105): its agent only knows the date it started on. Sent after a gap, or
/// once the date has changed, since the agent was last heard from (`heard_at`), and with every notice.
fn time_line(now: DateTime<Utc>, heard_at: Option<DateTime<Utc>>, notice: bool) -> Option<String> {
    let gap = heard_at
        .filter(|h| now - *h >= TIME_AFTER || h.date_naive() != now.date_naive())
        .map(|h| now - h);
    if gap.is_none() && !notice {
        return None;
    }
    let time = now.format("%Y-%m-%d %H:%M");
    Some(match gap {
        Some(gap) => format!(
            "[sloppod: it's {time} UTC, {} since the last message]",
            how_long(gap)
        ),
        None => format!("[sloppod: it's {time} UTC]"),
    })
}

/// A gap in the largest whole unit that fits it.
fn how_long(gap: chrono::Duration) -> String {
    let (n, unit) = if gap.num_hours() >= 48 {
        (gap.num_days(), "day")
    } else if gap.num_minutes() >= 60 {
        (gap.num_hours(), "hour")
    } else if gap.num_minutes() >= 1 {
        (gap.num_minutes(), "minute")
    } else {
        return "under a minute".to_owned();
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}
/// How a turn the hub sent ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEnd {
    Ended,
    /// It failed, with this error.
    Failed(String),
    /// It failed on the account's usage limit; the session carries on once that resets.
    Limited,
}

impl TurnEnd {
    fn of(result: &Result<Value, RpcError>) -> Self {
        match result {
            Ok(_) => TurnEnd::Ended,
            Err(e) if limits::is_usage_limit(e) => TurnEnd::Limited,
            Err(e) => TurnEnd::Failed(e.message.clone()),
        }
    }
}

/// What a session that waited for its usage limit is told when it carries on.
pub(crate) const LIMIT_RESET_NOTICE: &str = "sloppod: your last turn stopped at the account's usage limit, which should have \
     reset by now. Carry on where you left off.";

/// A batch of prompts on its way to the agent.
struct Held {
    key: Uuid,
    /// The request carrying it, once sent. After a restart, a held batch whose request the supervisor still has open
    /// reached the agent; any other is sent again.
    request: Option<i64>,
    /// Left by the runtime before a restart, waiting to learn whether it reached the agent.
    restored: bool,
    stored: Vec<Value>,
}

/// How a notice gets to the agent.
enum Delivery {
    Queued,
    Turn(Option<Arc<AgentConn>>, Queued, Option<Uuid>),
    Steer(Option<Uuid>),
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
    /// How many of the agent's commands its supervisor last said were running; `None` until it says, and from
    /// supervisors that don't.
    commands: Option<usize>,
    /// The agent is compacting its context; steers wait for it to end. A turn's end ends it too, in case the
    /// compaction's own end never came.
    compacting: bool,
    /// The agent's tool calls that have started and not yet completed or failed, by id; notices wait for them. A
    /// turn's end ends them too.
    tools: HashSet<String>,
    /// How many turns the hub sent have ended, and how the last one did. A watcher compares the count with the one it
    /// saw, so no turn's end is missed between its looks.
    turns_ended: u64,
    last_turn: Option<TurnEnd>,
    /// The session hit its usage limit: nothing starts a turn by itself until then, when it carries on (#251).
    limited_until: Option<DateTime<Utc>>,
    permissions: HashMap<u64, Permission>,
    next_permission: u64,
    queue: Vec<Queued>,
    /// Prompts taken from the queue, and notices, on their way to the agent: kept in the stored queue until the
    /// request carrying them is answered, so a restart before then doesn't lose them (#237).
    in_flight: Vec<Held>,
    /// For an agent a runtime restart cut off, if it has to be launched again rather than attached to.
    restart_notice: Option<String>,
    /// The session was stopped: nothing queued for it runs any more.
    stopped: bool,
}

impl Live {
    /// Nothing of the agent's work is under way any more: its turn ended, or it was replaced, parked or stopped.
    fn end_work(&self, state: &mut LiveState) {
        state.compacting = false;
        state.tools.clear();
        self.boundary.notify_waiters();
    }
}

pub struct Live {
    pub id: Uuid,
    events: broadcast::Sender<UiEvent>,
    state: Mutex<LiveState>,
    /// Held while the session is parked or woken, so the two never overlap.
    transition: Mutex<()>,
    /// Notified when something a steer waits for ends: a compaction, a tool call, a permission request, the turn.
    boundary: Notify,
    /// When the agent was last heard from, for telling it the time after a gap; `None` until this runtime knows.
    heard_at: StdMutex<Option<DateTime<Utc>>>,
}

/// What `run_queued` does with its batch.
enum Start {
    /// Runs it on the agent it was taken for, in this agent session.
    Run(String, Vec<Queued>),
    /// The session was stopped: its callers get an error.
    Stopped(Vec<Queued>),
    /// Back in the queue, and run on a newer agent that was up and idle.
    Requeued(Option<(Arc<AgentConn>, Vec<Queued>, Option<Uuid>)>),
    /// The first message couldn't be marked as taken, so running it could run it twice: nothing runs, its callers
    /// get an error, and the first message stays stored for the agent's next start.
    Failed(Vec<Queued>),
}

/// Why a session couldn't be parked.
#[derive(Debug, thiserror::Error)]
pub enum ParkError {
    #[error("the session is in a turn; stop it or let it finish before parking")]
    InTurn,
    #[error("the session is stopped")]
    Stopped,
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// Brings a parked session back; the runtime's is `lifecycle::wake`, which leaves other sessions alone.
pub type Waker =
    Arc<dyn Fn(Uuid) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> + Send + Sync>;

/// Called when a turn the hub sent ends well, e.g. to check what it ended on (ADR 26).
pub type TurnEnded = Arc<dyn Fn(Uuid) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Called with each `usage_update` an agent sends, e.g. to keep the rate limits it reports (#140).
pub type UsageSeen =
    Arc<dyn Fn(Uuid, Value) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Called when a session's turn hits its usage limit, to keep what depends on it (a review round's pause); says when
/// the limit resets, if that's known (#140's quotas).
pub type LimitHit =
    Arc<dyn Fn(Uuid) -> Pin<Box<dyn Future<Output = Option<DateTime<Utc>>> + Send>> + Send + Sync>;

/// Called when a session's wait for its usage limit ends, with when, to keep what depends on it (a review round's
/// pause).
pub type LimitEnded =
    Arc<dyn Fn(Uuid, DateTime<Utc>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub struct Hub {
    store: Arc<dyn SessionStore>,
    waker: StdMutex<Option<Waker>>,
    turn_ended: StdMutex<Option<TurnEnded>>,
    usage_seen: StdMutex<Option<UsageSeen>>,
    sessions: StdMutex<HashMap<Uuid, Arc<Live>>>,
    agent_capabilities: StdMutex<Option<Value>>,
    /// The time the tests say it is, instead of the clock's.
    now: StdMutex<Option<DateTime<Utc>>>,
    limit_hit: StdMutex<Option<LimitHit>>,
    limit_ended: StdMutex<Option<LimitEnded>>,
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
            turn_ended: StdMutex::default(),
            usage_seen: StdMutex::default(),
            sessions: StdMutex::default(),
            agent_capabilities: StdMutex::default(),
            limit_hit: StdMutex::default(),
            limit_ended: StdMutex::default(),
            now: StdMutex::default(),
        }
    }

    fn now(&self) -> DateTime<Utc> {
        self.now.lock().expect("hub lock").unwrap_or_else(Utc::now)
    }

    #[cfg(test)]
    pub(crate) fn set_now(&self, now: DateTime<Utc>) {
        *self.now.lock().expect("hub lock") = Some(now);
    }

    fn heard_from_agent(&self, id: Uuid) {
        *self.live(id).heard_at.lock().expect("hub lock") = Some(self.now());
    }

    /// `blocks` as the agent gets them: after a gap, and always for a notice, led by a line with the time.
    async fn with_time(&self, id: Uuid, blocks: Vec<Value>, notice: bool) -> Vec<Value> {
        let heard_at = self.heard_at(id).await;
        match time_line(self.now(), heard_at, notice) {
            Some(line) => std::iter::once(json!({"type": "text", "text": line}))
                .chain(blocks)
                .collect(),
            None => blocks,
        }
    }

    pub fn set_waker(&self, waker: Waker) {
        *self.waker.lock().unwrap() = Some(waker);
    }

    pub fn set_turn_ended(&self, turn_ended: TurnEnded) {
        *self.turn_ended.lock().unwrap() = Some(turn_ended);
    }

    pub fn set_usage_seen(&self, usage_seen: UsageSeen) {
        *self.usage_seen.lock().unwrap() = Some(usage_seen);
    }

    pub fn set_limit_hit(&self, limit_hit: LimitHit) {
        *self.limit_hit.lock().unwrap() = Some(limit_hit);
    }

    pub fn set_limit_ended(&self, limit_ended: LimitEnded) {
        *self.limit_ended.lock().unwrap() = Some(limit_ended);
    }

    /// A session's wait for its usage limit ended at `at`.
    async fn limit_ended(&self, id: Uuid, at: DateTime<Utc>) {
        let limit_ended = self.limit_ended.lock().unwrap().clone();
        if let Some(limit_ended) = limit_ended {
            limit_ended(id, at).await;
        }
    }

    /// The session's agent, if it's up.
    pub async fn agent(&self, id: Uuid) -> Option<Arc<AgentConn>> {
        self.agent_for(id).await.ok().map(|(agent, _)| agent)
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
                    boundary: Notify::new(),
                    heard_at: StdMutex::default(),
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

    /// When the agent was last heard from. Until this runtime has heard from it, that's the session's last activity,
    /// read before this runtime records or sends anything for the session.
    async fn heard_at(&self, id: Uuid) -> Option<DateTime<Utc>> {
        let live = self.live(id);
        let known = *live.heard_at.lock().expect("hub lock");
        if known.is_some() {
            return known;
        }
        let active_at = match self.store.get_session(id).await {
            Ok(row) => row?.active_at,
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "reading the session's last activity");
                return None;
            }
        };
        Some(
            *live
                .heard_at
                .lock()
                .expect("hub lock")
                .get_or_insert(active_at),
        )
    }

    /// Appends an update to the event log and sends it to every UI.
    pub async fn record(&self, id: Uuid, update: Value) {
        self.heard_at(id).await;
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
        let resolved = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            if let Some(old) = state.agent.replace(agent) {
                old.close();
            }
            state.agent_session_id = None;
            state.turn_running = false;
            state.commands = None;
            live.end_work(&mut state);
            // The new link reports the requests the agent still has open, and they're asked again under it.
            take_agent_permissions(&mut state)
        };
        self.resolve_permissions(id, resolved);
    }

    /// Tells the UIs that these permission requests are no longer open.
    fn resolve_permissions(&self, id: Uuid, resolved: Vec<(u64, Option<String>)>) {
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

    /// What the supervisor of `agent`, the session's current one, says about the agent's running commands.
    pub async fn set_commands(&self, id: Uuid, agent: &Arc<AgentConn>, running: usize) {
        let live = self.live(id);
        let mut state = live.state.lock().await;
        if state.agent.as_ref().is_some_and(|a| Arc::ptr_eq(a, agent)) {
            state.commands = Some(running);
        }
    }

    /// Whether anything is going on in the session: a turn, messages waiting, a question or permission request the
    /// user hasn't answered, or commands of the agent's still running. `None` when its supervisor doesn't say
    /// what's running, so only the rest is known.
    pub async fn busy(&self, id: Uuid) -> Option<bool> {
        let live = self.live(id);
        let state = live.state.lock().await;
        let busy = state.turn_running || !state.queue.is_empty() || !state.permissions.is_empty();
        match state.commands {
            _ if busy => Some(true),
            Some(running) => Some(running > 0),
            None => None,
        }
    }

    /// How many of the agent's commands are running, if its supervisor says.
    pub async fn commands(&self, id: Uuid) -> Option<usize> {
        self.live(id).state.lock().await.commands
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
        // What a restart left on its way to the agent: a turn the agent has open has it, a steer it has open waits
        // for its answer, and the rest is sent again.
        let open: Vec<(i64, bool)> = running
            .open
            .iter()
            .filter_map(|r| Some((r["id"].as_i64()?, r["method"] == "_session/steering")))
            .collect();
        {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            self.settle_restored(id, &mut state, &open).await;
        }
        let mut turn = None;
        for request in &running.open {
            if let Some(request_id) = request["id"].as_i64() {
                let reply = agent.adopt(request_id);
                if request["method"] == "session/prompt" {
                    turn = Some(reply);
                } else if request["method"] == "_session/steering" {
                    let hub = self.clone();
                    tokio::spawn(async move {
                        let answer = reply.await.unwrap_or_else(|_| {
                            Err(RpcError::new(-32002, "the session's agent disconnected"))
                        });
                        hub.steer_answered(id, request_id, answer).await;
                    });
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
            // An agent that came up but never got its first message (the link dropped before it ran) gets it now.
            None => {
                let pending = match self.store.pending_prompt(id).await {
                    Ok(pending) => pending,
                    Err(e) => {
                        tracing::warn!(session = %id, error = %e, "reading the first message");
                        None
                    }
                };
                self.agent_ready(
                    id,
                    agent,
                    running.ready.agent_session_id,
                    agent_state,
                    pending,
                )
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
                    let end = match turn.await {
                        Ok(result) => {
                            if let Err(e) = &result {
                                tracing::warn!(session = %id, error = %e, "adopted turn failed");
                            }
                            TurnEnd::of(&result)
                        }
                        Err(_) => TurnEnd::Ended,
                    };
                    hub.turn_finished(id, end).await;
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
            self.settle_restored(id, &mut state, &[]).await;
            // A session waiting for its usage limit keeps its queue until then.
            let queued = match state.limited_until {
                Some(_) => Vec::new(),
                None => std::mem::take(&mut state.queue),
            };
            let held = self.hold(id, &mut state, &queued).await;
            if pending.is_some() || !queued.is_empty() {
                state.turn_running = true;
            } else {
                self.set_phase(id, Phase::Idle).await;
            }
            (queued, held)
        };
        let (queued, held) = queued;
        if pending.is_none() && queued.is_empty() {
            return;
        }
        tokio::spawn(
            self.clone()
                .run_queued(id, agent.clone(), pending, queued, held),
        );
    }

    /// Runs queued prompts (and a prompt the agent was holding, if any) as one turn, and answers each prompt
    /// with its outcome. Boxed because a turn ending starts the next queued one.
    pub(crate) fn run_queued(
        self: Arc<Self>,
        id: Uuid,
        taken_for: Arc<AgentConn>,
        pending: Option<String>,
        queued: Vec<Queued>,
        held: Option<Uuid>,
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
            let notice = queued.iter().any(|q| q.notice);
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
                        let cleared = match pending {
                            Some(_) => self.store.clear_pending_prompt(id).await,
                            None => Ok(()),
                        };
                        match cleared {
                            Ok(()) => {
                                self.set_phase(id, Phase::Working).await;
                                Start::Run(agent_session_id, queued)
                            }
                            Err(e) => {
                                tracing::error!(session = %id, error = %e, "clearing the first message");
                                self.release(id, &mut state, held).await;
                                state.turn_running = false;
                                self.set_phase(id, Phase::Idle).await;
                                Start::Failed(queued)
                            }
                        }
                    }
                    _ if state.stopped => {
                        self.release(id, &mut state, held).await;
                        Start::Stopped(queued)
                    }
                    _ => {
                        // Taken for an agent that's gone: everything goes back, ahead of what's queued since,
                        // recorded already so not shown as waiting behind a turn. A newer agent that's up and idle
                        // runs it now. The first message isn't requeued: it's still stored, and the next agent to
                        // be ready reads it from there.
                        let requeued = queued.into_iter().map(|q| Queued { id: None, ..q });
                        let rest = std::mem::take(&mut state.queue);
                        state.queue = requeued.chain(rest).collect();
                        self.release(id, &mut state, held).await;
                        self.save_queue(id, &state).await;
                        let idle = state.agent_session_id.is_some()
                            && !state.turn_running
                            && !state.queue.is_empty();
                        match (state.agent.clone(), idle) {
                            (Some(agent), true) => {
                                state.turn_running = true;
                                let taken = std::mem::take(&mut state.queue);
                                let held = self.hold(id, &mut state, &taken).await;
                                Start::Requeued(Some((agent, taken, held)))
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
                Start::Failed(queued) => {
                    for reply in queued.into_iter().filter_map(|q| q.reply) {
                        let _ = reply.send(Err(RpcError::new(
                            -32603,
                            "the session's store failed; try again",
                        )));
                    }
                    return;
                }
                Start::Requeued(next) => {
                    if let Some((agent, queued, held)) = next {
                        tokio::spawn(self.clone().run_queued(id, agent, None, queued, held));
                    }
                    return;
                }
            };
            let agent = taken_for;
            let result = self
                .turn_on(id, &agent, &agent_session_id, blocks, notice, held)
                .await;
            if let Err(e) = &result {
                tracing::warn!(session = %id, error = %e, "queued prompt failed");
            }
            for reply in queued.into_iter().filter_map(|q| q.reply) {
                let _ = reply.send(result.clone());
            }
        })
    }

    /// Keeps the session's queue in the store as it is now, so a restart doesn't lose it (#39). Called under the
    /// session's state lock, right after the queue changes.
    async fn save_queue(&self, id: Uuid, state: &LiveState) {
        let stored: Vec<Value> = state
            .in_flight
            .iter()
            .flat_map(|held| {
                held.stored.iter().map(|q| {
                    let mut q = q.clone();
                    q["held"] = json!({"batch": held.key, "request": held.request});
                    q
                })
            })
            .chain(state.queue.iter().map(stored))
            .collect();
        if let Err(e) = self.store.save_queue(id, &stored).await {
            tracing::warn!(session = %id, error = %e, "keeping the session's queue");
        }
    }

    /// Keeps `batch` in the stored queue while it's on its way to the agent; under the state lock.
    async fn hold(&self, id: Uuid, state: &mut LiveState, batch: &[Queued]) -> Option<Uuid> {
        if batch.is_empty() {
            return None;
        }
        let key = Uuid::new_v4();
        state.in_flight.push(Held {
            key,
            request: None,
            restored: false,
            stored: batch.iter().map(stored).collect(),
        });
        self.save_queue(id, state).await;
        Some(key)
    }

    /// A held batch was sent as `request`.
    async fn sent(&self, id: Uuid, key: Option<Uuid>, request: i64) {
        let Some(key) = key else {
            return;
        };
        let live = self.live(id);
        let mut state = live.state.lock().await;
        if let Some(held) = state.in_flight.iter_mut().find(|h| h.key == key) {
            held.request = Some(request);
            self.save_queue(id, &state).await;
        }
    }

    /// Held batches a restart left, once the agent's open requests are known (`(id, whether it's a steer)`): a turn
    /// the agent has open has its batch; a steer it has open stays held until it's answered, as it may yet be
    /// refused; the rest go back to the front of the queue. Under the state lock.
    async fn settle_restored(&self, id: Uuid, state: &mut LiveState, open: &[(i64, bool)]) {
        if !state.in_flight.iter().any(|h| h.restored) {
            return;
        }
        let (restored, current): (Vec<Held>, Vec<Held>) = std::mem::take(&mut state.in_flight)
            .into_iter()
            .partition(|h| h.restored);
        state.in_flight = current;
        let open_as = |held: &Held| {
            held.request
                .and_then(|r| open.iter().find(|(o, _)| *o == r).map(|(_, steer)| *steer))
        };
        let (open_steers, rest): (Vec<Held>, Vec<Held>) =
            restored.into_iter().partition(|h| open_as(h) == Some(true));
        state
            .in_flight
            .extend(open_steers.into_iter().map(|h| Held {
                restored: false,
                ..h
            }));
        let resent = rest
            .into_iter()
            .filter(|h| open_as(h).is_none())
            .flat_map(|h| h.stored)
            .map(|q| unstored(&q));
        let since = std::mem::take(&mut state.queue);
        state.queue = resent.chain(since).collect();
        self.save_queue(id, state).await;
    }

    /// A held batch reached the agent, or went back to the queue; under the state lock.
    async fn release(&self, id: Uuid, state: &mut LiveState, key: Option<Uuid>) {
        let Some(key) = key else {
            return;
        };
        let before = state.in_flight.len();
        state.in_flight.retain(|h| h.key != key);
        if state.in_flight.len() != before {
            self.save_queue(id, state).await;
        }
    }

    /// A held notice that couldn't be delivered goes back to the queue, and runs now if the agent is idle, e.g.
    /// because the turn it was steered into ended meanwhile.
    async fn requeue_held(self: &Arc<Self>, id: Uuid, key: Uuid) {
        let next = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            let Some(held) = state.in_flight.iter().find(|h| h.key == key) else {
                return;
            };
            let back: Vec<Queued> = held.stored.iter().map(unstored).collect();
            state.queue.extend(back);
            self.release(id, &mut state, Some(key)).await;
            let idle = state.agent_session_id.is_some()
                && !state.turn_running
                && !state.stopped
                && state.limited_until.is_none();
            match (state.agent.clone(), idle) {
                (Some(agent), true) => {
                    state.turn_running = true;
                    let queued = std::mem::take(&mut state.queue);
                    let held = self.hold(id, &mut state, &queued).await;
                    Some((agent, queued, held))
                }
                _ => None,
            }
        };
        if let Some((agent, queued, held)) = next {
            tokio::spawn(self.clone().run_queued(id, agent, None, queued, held));
        }
    }

    /// The answer to a steer a restart left open: taken, it's let go; refused or failed, it goes back to the queue,
    /// as a live steer's would.
    async fn steer_answered(
        self: &Arc<Self>,
        id: Uuid,
        request: i64,
        answer: Result<Value, RpcError>,
    ) {
        let key = {
            let live = self.live(id);
            let state = live.state.lock().await;
            state
                .in_flight
                .iter()
                .find(|h| h.request == Some(request))
                .map(|h| h.key)
        };
        let Some(key) = key else {
            return;
        };
        match answer {
            Ok(result) if result["outcome"] != "promptRequired" => {
                let live = self.live(id);
                let mut state = live.state.lock().await;
                self.release(id, &mut state, Some(key)).await;
            }
            _ => self.requeue_held(id, key).await,
        }
    }

    /// Puts back the queues a restart left in the store: ahead of anything queued since, and run once each session's
    /// agent is up, waking a parked session for them as a new message would. Their callers are gone, so nobody
    /// waits on the outcome; the transcript shows it.
    pub async fn restore_queues(self: &Arc<Self>) {
        let queues = match self.store.queues().await {
            Ok(queues) => queues,
            Err(e) => {
                tracing::error!(error = %e, "loading queued messages");
                return;
            }
        };
        for (id, stored) in queues {
            // What was on its way to the agent waits, held, until its agent says what it has.
            let mut held: Vec<Held> = Vec::new();
            let mut restored: Vec<Queued> = Vec::new();
            for q in stored {
                if q["held"].is_object() {
                    let key = q["held"]["batch"]
                        .as_str()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_default();
                    let mut entry = q.clone();
                    if let Some(entry) = entry.as_object_mut() {
                        entry.remove("held");
                    }
                    match held.iter_mut().find(|h| h.key == key) {
                        Some(batch) => batch.stored.push(entry),
                        None => held.push(Held {
                            key,
                            request: q["held"]["request"].as_i64(),
                            restored: true,
                            stored: vec![entry],
                        }),
                    }
                } else {
                    restored.push(unstored(&q));
                }
            }
            if restored.is_empty() && held.is_empty() {
                continue;
            }
            {
                let live = self.live(id);
                let mut state = live.state.lock().await;
                let since = std::mem::take(&mut state.queue);
                state.queue = restored.into_iter().chain(since).collect();
                state.in_flight.extend(held);
                self.save_queue(id, &state).await;
            }
            self.broadcast_queue(id).await;
            // One waiting for its usage limit is woken when that ends.
            if self.limited_until(id).await.is_some() {
                continue;
            }
            if let Err(e) = self.wake_for_message(id).await {
                tracing::warn!(session = %id, error = %e.message, "waking a session for its queued messages");
            }
        }
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
            state.commands = None;
            live.end_work(&mut state);
            self.set_phase(id, Phase::Disconnected).await;
            take_agent_permissions(&mut state)
        };
        self.resolve_permissions(id, resolved);
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
            self.heard_from_agent(id);
            if update["sessionUpdate"] == "usage_update"
                && let Some(usage_seen) = self.usage_seen.lock().unwrap().clone()
            {
                tokio::spawn(usage_seen(id, update.clone()));
            }
            if update["sessionUpdate"] == "current_mode_update"
                && let Some(mode) = update.get("currentModeId")
            {
                state.agent_state["modes"]["currentModeId"] = mode.clone();
            }
            if update["sessionUpdate"] == "compaction_update" {
                if update["status"] == "in_progress" {
                    state.compacting = true;
                } else {
                    state.compacting = false;
                    live.boundary.notify_waiters();
                }
            }
            if let Some(tool) = update["toolCallId"].as_str() {
                match update["status"].as_str() {
                    Some("completed" | "failed") => {
                        if state.tools.remove(tool) {
                            live.boundary.notify_waiters();
                        }
                    }
                    Some("pending" | "in_progress") => {
                        state.tools.insert(tool.to_owned());
                    }
                    _ if update["sessionUpdate"] == "tool_call" => {
                        state.tools.insert(tool.to_owned());
                    }
                    _ => {}
                }
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

    /// The first answer to a permission request wins; later ones are ignored. `by` says who answered, for the log.
    pub async fn answer_permission(
        self: &Arc<Self>,
        id: Uuid,
        pid: u64,
        result: Result<Value, RpcError>,
        by: &str,
    ) {
        let permission = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            let permission = state.permissions.remove(&pid);
            let blocked = |p: &Permission| matches!(p.asker, Asker::Agent { .. });
            let agent_asked = permission.as_ref().is_some_and(blocked);
            if agent_asked {
                live.boundary.notify_waiters();
            }
            if agent_asked && !state.permissions.values().any(blocked) && state.agent.is_some() {
                let phase = if state.turn_running {
                    Phase::Working
                } else {
                    Phase::Idle
                };
                self.set_phase(id, phase).await;
            }
            permission
        };
        let Some(permission) = permission else {
            tracing::debug!(session = %id, permission = pid, by, "ignoring an answer to a settled permission request");
            return;
        };
        let tool_call_id = tool_call_id(&permission.params);
        tracing::info!(
            session = %id,
            permission = pid,
            method = %permission.method,
            tool_call = tool_call_id.as_deref().unwrap_or_default(),
            outcome = %answer_outcome(&result),
            by,
            "permission request answered"
        );
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
        // A UI sends a message again when it can't tell whether the first send arrived, e.g. after a reload; it
        // doesn't run twice. A UI can't always tell: it only has the latest of a long session's history.
        let received = match message_id {
            Some(message_id) => self
                .store
                .has_message(id, message_id)
                .await
                .map_err(|e| RpcError::new(-32603, e.to_string()))?,
            None => false,
        };
        let (queued, behind_turn) = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            if received || message_id.is_some_and(|m| state.queue.iter().any(|q| q.id == Some(m))) {
                return Ok(
                    json!({"stopReason": "end_turn", "_meta": {"sloppod": {"alreadyReceived": true}}}),
                );
            }
            let agent_up = state.agent.is_some() && state.agent_session_id.is_some();
            // While the session waits for its usage limit, messages queue as they do behind a turn.
            if agent_up && !state.turn_running && state.limited_until.is_none() {
                state.turn_running = true;
                (None, false)
            } else {
                // Shown as waiting, and not waking a parked session, unless it waits only for the agent to be up.
                let waiting = agent_up || state.limited_until.is_some();
                let (reply, receiver) = oneshot::channel();
                state.queue.push(Queued {
                    id: waiting.then(|| message_id.unwrap_or_else(Uuid::new_v4)),
                    blocks: blocks.clone(),
                    notice: false,
                    reply: Some(reply),
                });
                self.save_queue(id, &state).await;
                (Some(receiver), waiting)
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
        self.turn_on(id, &agent, &agent_session_id, blocks, false, None)
            .await
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

    /// Runs a turn on `agent`; `held`, what it carries, stays kept until the agent answers.
    async fn turn_on(
        self: &Arc<Self>,
        id: Uuid,
        agent: &Arc<AgentConn>,
        agent_session_id: &str,
        blocks: Vec<Value>,
        notice: bool,
        held: Option<Uuid>,
    ) -> Result<Value, RpcError> {
        let prompt = self.with_time(id, blocks, notice).await;
        let (request, answer) = agent.send_request(
            "session/prompt",
            json!({"sessionId": agent_session_id, "prompt": prompt}),
        );
        self.sent(id, held, request).await;
        let result = answer.await;
        {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            self.release(id, &mut state, held).await;
        }
        self.turn_finished(id, TurnEnd::of(&result)).await;
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
            live.end_work(&mut state);
            let agent_up = state.agent_session_id.is_some();
            state.turn_running =
                agent_up && !state.queue.is_empty() && state.limited_until.is_none();
            let next = state.turn_running.then(|| std::mem::take(&mut state.queue));
            let held = match &next {
                Some(queued) => self.hold(id, &mut state, queued).await,
                None => None,
            };
            if next.is_none() && agent_up {
                self.set_phase(id, Phase::Idle).await;
            }
            next.map(|queued| (queued, held))
        };
        agent.sync_transcript();
        if let Some((queued, held)) = next {
            self.broadcast_queue(id).await;
            tokio::spawn(
                self.clone()
                    .run_queued(id, agent.clone(), None, queued, held),
            );
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
        // Waiting for its usage limit with no agent to steer into, the session carries on now, with all it has queued.
        let try_now = {
            let live = self.live(id);
            let state = live.state.lock().await;
            let up = state.agent.is_some() && state.agent_session_id.is_some();
            !up && state.limited_until.is_some() && state.queue.iter().any(|q| q.id == Some(qid))
        };
        if try_now {
            self.move_limit(id, self.now()).await;
            return Ok(json!({"outcome": "queued"}));
        }
        let queued = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            let index = state
                .queue
                .iter()
                .position(|q| q.id == Some(qid))
                .ok_or_else(|| RpcError::new(-32002, "that message isn't queued any more"))?;
            let queued = state.queue.remove(index);
            self.save_queue(id, &state).await;
            queued
        };
        self.broadcast_queue(id).await;
        self.record_user_message(id, &json!(queued.blocks), true, queued.id)
            .await;
        let result = self.steer_in(id, queued.blocks, false, None).await;
        if let Some(reply) = queued.reply {
            let _ = reply.send(result.clone());
        }
        result
    }

    /// Tells the agent something happened outside its turns, e.g. a proposal was decided: a new turn when it's
    /// idle, steered into the running one, or queued until it's up.
    /// The agent's pod is going away: from now on the agent counts as not up, so new prompts and notices, and
    /// whatever its running turn would hand on when it ends, wait for the agent that replaces it.
    pub async fn retire_agent(&self, id: Uuid, agent: &Arc<AgentConn>) {
        let live = self.live(id);
        let mut state = live.state.lock().await;
        if state.agent.as_ref().is_some_and(|a| Arc::ptr_eq(a, agent)) {
            state.agent_session_id = None;
        }
    }

    /// How many of the session's turns have ended, and whether one is running, read together.
    pub async fn turn_state(&self, id: Uuid) -> (u64, bool) {
        let live = self.live(id);
        let state = live.state.lock().await;
        (state.turns_ended, state.turn_running)
    }

    /// Sends a notice as a new turn, but only while the session is idle: the agent up, no turn running, and, if
    /// given, `turns_ended` turns ended, as the sender last saw it. Checked and the turn reserved under one lock,
    /// so nothing can start in between; says whether it was sent.
    pub async fn notify_when_idle(
        self: &Arc<Self>,
        id: Uuid,
        turns_ended: Option<u64>,
        text: String,
    ) -> bool {
        let notice = notice_queued(&text);
        let (agent, held) = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            let up = state.agent.is_some() && state.agent_session_id.is_some();
            if !up
                || state.turn_running
                || state.limited_until.is_some()
                || turns_ended.is_some_and(|n| n != state.turns_ended)
            {
                return false;
            }
            state.turn_running = true;
            let held = self
                .hold(id, &mut state, std::slice::from_ref(&notice))
                .await;
            (state.agent.clone(), held)
        };
        self.record_notice(id, &notice.blocks[0], None).await;
        if let Some(agent) = agent {
            tokio::spawn(self.clone().run_queued(id, agent, None, vec![notice], held));
        }
        true
    }

    /// Tells the agent something: a new turn when it's idle, steered into the running one, or queued until it's
    /// up. Kept in the stored queue until it reaches the agent, so a restart doesn't lose it (#237).
    pub async fn notify(self: &Arc<Self>, id: Uuid, text: String) {
        self.notify_summarised(id, text, None).await;
    }

    /// A notice the UI shows folded as `summary`, e.g. a message from another session; the agent gets all of it.
    pub async fn notify_summarised(
        self: &Arc<Self>,
        id: Uuid,
        text: String,
        summary: Option<String>,
    ) {
        let notice = notice_queued(&text);
        let blocks = notice.blocks.clone();
        let delivery = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            let up = state.agent.is_some() && state.agent_session_id.is_some();
            if !up || (state.limited_until.is_some() && !state.turn_running) {
                state.queue.push(notice);
                self.save_queue(id, &state).await;
                Delivery::Queued
            } else if state.turn_running {
                let held = self
                    .hold(id, &mut state, std::slice::from_ref(&notice))
                    .await;
                Delivery::Steer(held)
            } else {
                state.turn_running = true;
                let held = self
                    .hold(id, &mut state, std::slice::from_ref(&notice))
                    .await;
                Delivery::Turn(state.agent.clone(), notice, held)
            }
        };
        self.record_notice(id, &blocks[0], summary.as_deref()).await;
        match delivery {
            Delivery::Queued => {}
            Delivery::Turn(agent, notice, held) => {
                if let Some(agent) = agent {
                    tokio::spawn(self.clone().run_queued(id, agent, None, vec![notice], held));
                }
            }
            Delivery::Steer(held) => {
                let hub = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = hub.steer_in(id, blocks.clone(), true, held).await {
                        tracing::warn!(session = %id, error = %e, "steering a notice in");
                        if let Some(key) = held {
                            hub.requeue_held(id, key).await;
                        }
                    }
                });
            }
        }
    }

    /// Shows a notice in the session's log as a message to the agent.
    async fn record_notice(&self, id: Uuid, block: &Value, summary: Option<&str>) {
        let mut sloppod = json!({"notice": true});
        if let Some(summary) = summary {
            sloppod["summary"] = json!(summary);
        }
        self.record(
            id,
            json!({
                "sessionUpdate": "user_message_chunk",
                "messageId": Uuid::new_v4(),
                "content": block,
                "_meta": {"sloppod": sloppod},
            }),
        )
        .await;
    }

    /// Steers blocks into the running turn. If that turn has just ended they start a new one here, so the turn
    /// is tracked like any other rather than run detached by the adapter. `held` is released once they reach the
    /// agent. A `notice` waits for a moment between the agent's tool calls (see `before_steer`).
    async fn steer_in(
        self: &Arc<Self>,
        id: Uuid,
        blocks: Vec<Value>,
        notice: bool,
        held: Option<Uuid>,
    ) -> Result<Value, RpcError> {
        for _ in 0..10 {
            self.before_steer(id, notice).await;
            let (agent, agent_session_id) = self.agent_for(id).await?;
            let prompt = self.with_time(id, blocks.clone(), notice).await;
            let (request, answer) = agent.send_request(
                "_session/steering",
                json!({
                    "sessionId": agent_session_id,
                    "prompt": prompt,
                    "_meta": {"steering": {"idleBehavior": "promptRequired"}},
                }),
            );
            self.sent(id, held, request).await;
            let result = answer.await?;
            if result["outcome"] != "promptRequired" {
                let live = self.live(id);
                let mut state = live.state.lock().await;
                self.release(id, &mut state, held).await;
                return Ok(result);
            }
            let start = {
                let live = self.live(id);
                let mut state = live.state.lock().await;
                !std::mem::replace(&mut state.turn_running, true)
            };
            if start {
                let queued = vec![Queued {
                    id: None,
                    blocks,
                    notice,
                    reply: None,
                }];
                tokio::spawn(self.clone().run_queued(id, agent, None, queued, held));
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

    /// A steer pre-empts whatever the agent is doing (the adapter doesn't let us ask it to wait), and a compaction it
    /// pre-empts is cancelled rather than resumed. So steers wait for one to end, however long it takes. A notice
    /// also waits for the agent's tool calls and permission requests to end, or its turn: one it pre-empts is
    /// cancelled, which the agent reads as the user telling it to stop. The user's own steers go in at once.
    async fn before_steer(&self, id: Uuid, notice: bool) {
        let live = self.live(id);
        loop {
            let changed = live.boundary.notified();
            {
                let state = live.state.lock().await;
                let busy = notice
                    && (!state.tools.is_empty()
                        || state
                            .permissions
                            .values()
                            .any(|p| matches!(p.asker, Asker::Agent { .. })));
                if !state.compacting && !busy {
                    return;
                }
            }
            changed.await;
        }
    }

    /// Delivers a message into the running turn; before the agent is up it joins the queue instead.
    pub async fn steer(self: &Arc<Self>, id: Uuid, params: Value) -> Result<Value, RpcError> {
        let blocks = params["prompt"].as_array().cloned().unwrap_or_default();
        let message_id = client_message_id(&params);
        if self.agent_for(id).await.is_err() {
            {
                let live = self.live(id);
                let mut state = live.state.lock().await;
                state.queue.push(Queued {
                    id: None,
                    blocks,
                    notice: false,
                    reply: None,
                });
                self.save_queue(id, &state).await;
            }
            self.record_user_message(id, &params["prompt"], false, message_id)
                .await;
            self.wake_for_message(id).await?;
            return Ok(json!({"outcome": "queued"}));
        }
        self.record_user_message(id, &params["prompt"], true, message_id)
            .await;
        self.steer_in(id, blocks, false, None).await
    }

    /// Cancels the running turn; `by` says who did, for the log.
    pub async fn cancel(self: &Arc<Self>, id: Uuid, by: &str) {
        tracing::info!(session = %id, by, "cancelling the turn");
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
            self.answer_permission(id, pid, Ok(answer), &format!("the cancel by {by}"))
                .await;
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

    /// Up to `limit` of the session's latest recorded updates before `before`. A UI gets the latest when it loads a
    /// session and older pages as it asks for them, so neither it nor the runtime holds a long session's whole log.
    pub async fn events_before(
        &self,
        id: Uuid,
        before: i64,
        limit: i64,
    ) -> Result<Vec<(i64, Value)>, sqlx::Error> {
        self.store.events_before(id, before, limit).await
    }

    /// The sequence number of the session's latest recorded update; 0 when it has none.
    pub async fn last_event(&self, id: Uuid) -> Result<i64, sqlx::Error> {
        self.store.last_event(id).await
    }

    /// Everything a UI needs when it loads a session, but the log.
    pub async fn replay(&self, id: Uuid) -> Result<Replay, sqlx::Error> {
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

    /// Counts a turn the hub sent as ended. One that hit the usage limit leaves the session waiting for the reset;
    /// one that ended well ends any such wait.
    async fn turn_finished(self: &Arc<Self>, id: Uuid, end: TurnEnd) {
        self.heard_from_agent(id);
        let now = self.now();
        let limit_hit = self.limit_hit.lock().unwrap().clone();
        let reset = match (&end, limit_hit) {
            (TurnEnd::Limited, Some(limit_hit)) => limit_hit(id).await,
            _ => None,
        };
        let limited = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            state.turns_ended += 1;
            state.last_turn = Some(end.clone());
            let until = match end {
                TurnEnd::Limited => Some(limits::wait_until(reset, now)),
                TurnEnd::Ended => None,
                TurnEnd::Failed(_) => state.limited_until,
            };
            let changed = until != state.limited_until;
            if changed {
                state.limited_until = until;
                self.save_limit(id, until).await;
            }
            (until.filter(|_| changed), changed && until.is_none())
        };
        let (limited, ended) = limited;
        if ended {
            self.limit_ended(id, now).await;
        }
        if let Some(until) = limited {
            tracing::info!(session = %id, %until, "the session hit its usage limit; it carries on then");
            self.resume_at(id, until);
        }
        let turn_ended = self.turn_ended.lock().unwrap().clone();
        if let (TurnEnd::Ended, Some(turn_ended)) = (end, turn_ended) {
            tokio::spawn(turn_ended(id));
        }
    }

    /// Keeps when a session waiting for its usage limit carries on, and tells the UIs; under the state lock.
    async fn save_limit(&self, id: Uuid, until: Option<DateTime<Utc>>) {
        match self.store.set_limited_until(id, until).await {
            Ok(Some(row)) => self.broadcast(
                id,
                UiEvent::Notification {
                    seq: None,
                    message: notification("_sloppod/session", json!({"session": row.to_api()})),
                },
            ),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "keeping the session's usage limit")
            }
        }
    }

    /// When the session waiting for its usage limit is to carry on, if it is.
    pub async fn limited_until(&self, id: Uuid) -> Option<DateTime<Utc>> {
        self.live(id).state.lock().await.limited_until
    }

    /// Moves a session's wait for its usage limit to end at `until`, e.g. now that its credential isn't limited any
    /// more; a session that isn't waiting is left alone.
    pub async fn move_limit(self: &Arc<Self>, id: Uuid, until: DateTime<Utc>) {
        {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            if state.limited_until.is_none_or(|current| current == until) {
                return;
            }
            state.limited_until = Some(until);
            self.save_limit(id, Some(until)).await;
        }
        self.resume_at(id, until);
    }

    /// Carries the session on at `until`, unless its wait has ended or moved meanwhile.
    fn resume_at(self: &Arc<Self>, id: Uuid, until: DateTime<Utc>) {
        let hub = self.clone();
        tokio::spawn(async move {
            let wait = (until - hub.now()).to_std().unwrap_or_default();
            tokio::time::sleep(wait).await;
            hub.resume_after_limit(id, until).await;
        });
    }

    /// Ends a session's wait for its usage limit, if it's still the one that was to end at `until`: the session is
    /// told to carry on, in one turn with whatever queued meanwhile. If that turn hits the limit again, it waits again.
    pub(crate) async fn resume_after_limit(self: &Arc<Self>, id: Uuid, until: DateTime<Utc>) {
        let notice = notice_queued(LIMIT_RESET_NOTICE);
        let block = notice.blocks[0].clone();
        let (start, wake) = {
            let live = self.live(id);
            let mut state = live.state.lock().await;
            if state.limited_until != Some(until) || state.stopped {
                return;
            }
            // A turn the user started meanwhile carries on by itself, and the queue runs after it.
            if state.turn_running {
                state.limited_until = None;
                self.save_limit(id, None).await;
                (None, false)
            } else {
                let up = state.agent.is_some() && state.agent_session_id.is_some();
                // Kept before the wait is cleared, so a restart in between still carries on; one kept by a restart
                // that came after it isn't queued again.
                let kept = state.queue.iter().any(|q| q.blocks.first() == Some(&block));
                if !kept {
                    state.queue.insert(0, notice);
                }
                let start = if up {
                    state.turn_running = true;
                    let queued = std::mem::take(&mut state.queue);
                    let held = self.hold(id, &mut state, &queued).await;
                    state.agent.clone().map(|agent| (agent, queued, held))
                } else {
                    self.save_queue(id, &state).await;
                    None
                };
                state.limited_until = None;
                self.save_limit(id, None).await;
                (start, !up)
            }
        };
        self.limit_ended(id, self.now()).await;
        self.record_notice(id, &block, None).await;
        if let Some((agent, queued, held)) = start {
            self.broadcast_queue(id).await;
            tokio::spawn(self.clone().run_queued(id, agent, None, queued, held));
        }
        if wake && let Err(e) = self.wake_for_message(id).await {
            tracing::warn!(session = %id, error = %e.message, "waking a session whose usage limit reset");
        }
    }

    /// Puts back the waits for usage limits a restart left, and returns them for [`Self::resume_limits`] once the
    /// queues are back too, so none carries on, and keeps its queue, before its queue is restored.
    pub async fn restore_limits(&self) -> Vec<(Uuid, DateTime<Utc>)> {
        let limited = match self.store.limited_sessions().await {
            Ok(limited) => limited,
            Err(e) => {
                tracing::error!(error = %e, "loading sessions waiting for their usage limit");
                return Vec::new();
            }
        };
        for (id, until) in &limited {
            self.live(*id).state.lock().await.limited_until = Some(*until);
        }
        limited
    }

    /// Has the restored waits carry on when they were to.
    pub fn resume_limits(self: &Arc<Self>, limited: Vec<(Uuid, DateTime<Utc>)>) {
        for (id, until) in limited {
            self.resume_at(id, until);
        }
    }

    /// How many of the session's turns have ended, and how the last one did.
    pub async fn turns_ended(&self, id: Uuid) -> (u64, Option<TurnEnd>) {
        let live = self.live(id);
        let state = live.state.lock().await;
        (state.turns_ended, state.last_turn.clone())
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
            live.end_work(&mut state);
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

    /// Replaces a quiet session's pod (#28), and says whether it did. Under the session's transition lock, and only
    /// if it's idle with nothing going on (no turn, nothing queued, no open request, none of the agent's commands
    /// known to be running), its agent stops getting work: what comes in meanwhile waits for the agent in the new
    /// pod. Then `replace` ends the pod. If that fails, the agent is given back.
    pub async fn replace_pod<F, Fut>(self: &Arc<Self>, id: Uuid, replace: F) -> anyhow::Result<bool>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<()>>,
    {
        let live = self.live(id);
        let _transition = live.transition.lock().await;
        let idle = self
            .store
            .get_session(id)
            .await?
            .is_some_and(|row| row.phase() == Phase::Idle);
        if !idle {
            return Ok(false);
        }
        let (agent, agent_session_id) = {
            let mut state = live.state.lock().await;
            let busy = state.turn_running
                || !state.queue.is_empty()
                || !state.permissions.is_empty()
                || state.commands.is_some_and(|running| running > 0);
            if busy || state.agent_session_id.is_none() {
                return Ok(false);
            }
            (state.agent.clone(), state.agent_session_id.take())
        };
        if let Err(e) = replace().await {
            self.unpark(id, agent, agent_session_id).await;
            return Err(e);
        }
        Ok(true)
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
                let held = self.hold(id, &mut state, &queued).await;
                state.turn_running = true;
                self.set_phase(id, Phase::Working).await;
                tokio::spawn(self.clone().run_queued(id, agent, None, queued, held));
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
            live.end_work(&mut state);
            let queued = std::mem::take(&mut state.queue);
            state.in_flight.clear();
            self.save_queue(id, &state).await;
            (state.agent.take(), queued)
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

/// What a permission request or question was answered with, for the log.
fn answer_outcome(result: &Result<Value, RpcError>) -> String {
    match result {
        Ok(answer) => match (
            answer["outcome"]["outcome"].as_str(),
            answer["action"].as_str(),
        ) {
            (Some("selected"), _) => format!("selected {}", answer["outcome"]["optionId"]),
            (Some(outcome), _) | (None, Some(outcome)) => outcome.to_owned(),
            (None, None) => answer.to_string(),
        },
        Err(e) => format!("error {}: {}", e.code, e.message),
    }
}

/// Removes the agent's permission requests, whose agent connection is gone: their ids and tool calls.
fn take_agent_permissions(state: &mut LiveState) -> Vec<(u64, Option<String>)> {
    state
        .permissions
        .extract_if(|_, p| matches!(p.asker, Asker::Agent { .. }))
        .map(|(pid, p)| (pid, tool_call_id(&p.params)))
        .collect()
}

fn tool_call_id(params: &Value) -> Option<String> {
    params["toolCall"]["toolCallId"]
        .as_str()
        .or(params["toolCallId"].as_str())
        .map(str::to_owned)
}
