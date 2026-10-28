//! The hub against an in-memory store and a scripted agent: what a turn, a queue, a steer and a notice send to the
//! agent and record in the session's log.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sloppod_protocol::link::{AgentReady, RunningAgent, ToSupervisor};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::db::SessionRow;
use crate::link::{AgentConn, RpcError, from_agent};
use crate::session::Hub;
use crate::store::SessionStore;
use crate::store::memory::MemoryStore;

/// What the harness's clock says, and when its session was last active.
const START: &str = "2026-10-27T15:40:00Z";

struct Harness {
    hub: Arc<Hub>,
    store: Arc<MemoryStore>,
    id: Uuid,
    agent: Arc<AgentConn>,
    to_agent: mpsc::UnboundedReceiver<ToSupervisor>,
}

impl Harness {
    /// A session whose agent is attached but hasn't finished starting.
    async fn starting() -> Self {
        let store = Arc::new(MemoryStore::default());
        let id = Uuid::new_v4();
        let mut row = SessionRow::for_test(id, "dev");
        row.active_at = START.parse().unwrap();
        store.add_session(row);
        let hub = Arc::new(Hub::new(store.clone()));
        hub.set_now(START.parse().unwrap());
        let (tx, to_agent) = mpsc::unbounded_channel();
        let agent = Arc::new(AgentConn::new(tx));
        hub.attach_agent(id, agent.clone()).await;
        Self {
            hub,
            store,
            id,
            agent,
            to_agent,
        }
    }

    /// A session whose agent is up and idle.
    async fn ready() -> Self {
        let harness = Self::starting().await;
        harness.agent_ready().await;
        harness
    }

    /// A session whose agent outlived an earlier link, with the requests still open in each direction.
    async fn adopted(open: Vec<Value>, waiting: Vec<Value>) -> Self {
        let harness = Self::starting().await;
        harness
            .hub
            .adopt_agent(
                harness.id,
                &harness.agent,
                RunningAgent {
                    ready: AgentReady {
                        agent_session_id: "agent-session".into(),
                        capabilities: json!({}),
                    },
                    open,
                    waiting,
                },
            )
            .await;
        harness
    }

    async fn agent_ready(&self) {
        self.hub
            .agent_ready(
                self.id,
                &self.agent,
                "agent-session".into(),
                json!({}),
                None,
            )
            .await;
    }

    /// The next ACP message the hub sends the agent.
    async fn next(&mut self) -> Value {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), self.to_agent.recv())
                .await
                .expect("the hub sent the agent nothing")
                .expect("the link closed");
            if let ToSupervisor::Acp { message } = message {
                return message;
            }
        }
    }

    /// Answers a request the agent was sent.
    async fn respond(&self, request: &Value, result: Value) {
        let response = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
        from_agent(&self.hub, self.id, &self.agent, response).await;
    }

    fn prompt(&self, text: &str) -> JoinHandle<Result<Value, RpcError>> {
        let (hub, id) = (self.hub.clone(), self.id);
        let params = json!({"sessionId": id, "prompt": [{"type": "text", "text": text}]});
        tokio::spawn(async move { hub.prompt(id, params).await })
    }

    /// The text of the user messages recorded so far, with whether each was steered in.
    fn user_messages(&self) -> Vec<(String, bool)> {
        self.store
            .updates(self.id)
            .iter()
            .filter(|u| u["sessionUpdate"] == "user_message_chunk")
            .map(|u| {
                (
                    u["content"]["text"].as_str().unwrap_or_default().to_owned(),
                    u["_meta"]["sloppod"]["steering"] == true,
                )
            })
            .collect()
    }

    async fn phase(&self) -> String {
        self.store
            .get_session(self.id)
            .await
            .unwrap()
            .expect("session")
            .phase
    }

    /// The ids of the messages waiting behind the running turn.
    async fn queued(&self) -> Vec<String> {
        let replay = self.hub.replay(self.id).await.unwrap();
        replay.queue["params"]["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| m["id"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

/// The text of what a request sent the agent, without the line telling it the time.
fn prompt_text(request: &Value) -> Vec<String> {
    let mut text = all_text(request);
    if time_line(request).is_some() {
        text.remove(0);
    }
    text
}

fn all_text(request: &Value) -> Vec<String> {
    request["params"]["prompt"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|block| block["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The line telling the agent the time that a request leads with, if it does.
fn time_line(request: &Value) -> Option<String> {
    all_text(request)
        .into_iter()
        .next()
        .filter(|text| text.starts_with("[sloppod: it's "))
}

/// Waits until the hub has queued `n` messages; prompting happens on another task.
async fn wait_for_queue(harness: &Harness, n: usize) -> Vec<String> {
    for _ in 0..100 {
        let queued = harness.queued().await;
        if queued.len() == n {
            return queued;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the queue never reached {n}");
}

#[tokio::test]
async fn a_prompt_runs_a_turn() {
    let mut h = Harness::ready().await;
    let turn = h.prompt("Fix the build");
    let request = h.next().await;
    assert_eq!(request["method"], "session/prompt");
    assert_eq!(request["params"]["sessionId"], "agent-session");
    assert_eq!(prompt_text(&request), ["Fix the build"]);
    assert_eq!(h.phase().await, "working");
    assert_eq!(h.user_messages(), [("Fix the build".to_owned(), false)]);

    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    let result = turn.await.unwrap().unwrap();
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(h.phase().await, "idle");
}

#[tokio::test]
async fn a_message_sent_during_a_turn_waits_and_runs_next() {
    let mut h = Harness::ready().await;
    let first = h.prompt("First");
    let first_request = h.next().await;

    let second = h.prompt("Second");
    wait_for_queue(&h, 1).await;
    // Waiting messages aren't in the log until they run.
    assert_eq!(h.user_messages(), [("First".to_owned(), false)]);

    h.respond(&first_request, json!({"stopReason": "end_turn"}))
        .await;
    first.await.unwrap().unwrap();
    let second_request = h.next().await;
    assert_eq!(second_request["method"], "session/prompt");
    assert_eq!(prompt_text(&second_request), ["Second"]);
    assert!(h.queued().await.is_empty());
    assert_eq!(
        h.user_messages(),
        [("First".to_owned(), false), ("Second".to_owned(), false)]
    );

    h.respond(&second_request, json!({"stopReason": "end_turn"}))
        .await;
    second.await.unwrap().unwrap();
    assert_eq!(h.phase().await, "idle");
}

#[tokio::test]
async fn a_queued_message_can_be_steered_into_the_turn() {
    let mut h = Harness::ready().await;
    let first = h.prompt("First");
    let first_request = h.next().await;
    let second = h.prompt("Actually, use the other approach");
    let queued = wait_for_queue(&h, 1).await;

    let (hub, id) = (h.hub.clone(), h.id);
    let qid = queued[0].clone();
    let steer = tokio::spawn(async move { hub.steer_queued(id, json!({"id": qid})).await });
    let steering = h.next().await;
    assert_eq!(steering["method"], "_session/steering");
    assert_eq!(prompt_text(&steering), ["Actually, use the other approach"]);
    h.respond(&steering, json!({"outcome": "injected"})).await;
    steer.await.unwrap().unwrap();

    // The steered message is in the log as a steer, and its own prompt is answered.
    assert!(h.queued().await.is_empty());
    assert_eq!(
        h.user_messages(),
        [
            ("First".to_owned(), false),
            ("Actually, use the other approach".to_owned(), true)
        ]
    );
    second.await.unwrap().unwrap();

    h.respond(&first_request, json!({"stopReason": "end_turn"}))
        .await;
    first.await.unwrap().unwrap();
    assert_eq!(h.phase().await, "idle");
}

#[tokio::test]
async fn a_queued_message_can_be_taken_back() {
    let mut h = Harness::ready().await;
    let first = h.prompt("First");
    let first_request = h.next().await;
    let kept = h.prompt("Kept");
    wait_for_queue(&h, 1).await;
    let taken = h.prompt("Taken back");
    let queued = wait_for_queue(&h, 2).await;

    let removed = h
        .hub
        .remove_queued(h.id, json!({"id": queued[1]}))
        .await
        .unwrap();
    assert_eq!(removed["prompt"][0]["text"], "Taken back");
    assert_eq!(taken.await.unwrap().unwrap()["stopReason"], "cancelled");
    assert_eq!(h.queued().await, [queued[0].clone()]);
    let stored = h.store.queues().await.unwrap();
    let texts: Vec<&str> = stored[0]
        .1
        .iter()
        .filter_map(|m| m["blocks"][0]["text"].as_str())
        .collect();
    assert_eq!(texts, ["Kept"]);
    assert!(
        h.hub
            .remove_queued(h.id, json!({"id": queued[1]}))
            .await
            .is_err()
    );
    // A UI that didn't hear it was taken back sends it again; it doesn't run.
    let resent = h
        .hub
        .prompt(
            h.id,
            json!({"sessionId": h.id, "prompt": [{"type": "text", "text": "Taken back"}],
                "_meta": {"sloppod": {"messageId": queued[1]}}}),
        )
        .await
        .unwrap();
    assert_eq!(resent["stopReason"], "cancelled");
    assert_eq!(h.queued().await, [queued[0].clone()]);

    // It never runs; the rest of the queue still does.
    h.respond(&first_request, json!({"stopReason": "end_turn"}))
        .await;
    first.await.unwrap().unwrap();
    let next = h.next().await;
    assert_eq!(prompt_text(&next), ["Kept"]);
    h.respond(&next, json!({"stopReason": "end_turn"})).await;
    kept.await.unwrap().unwrap();
    assert_eq!(
        h.user_messages(),
        [("First".to_owned(), false), ("Kept".to_owned(), false)]
    );
}

#[tokio::test]
async fn a_notice_starts_a_turn_when_the_agent_is_idle() {
    let mut h = Harness::ready().await;
    h.hub
        .notify(h.id, "sloppod: your proposal was approved.".into())
        .await;
    let request = h.next().await;
    assert_eq!(request["method"], "session/prompt");
    assert_eq!(
        prompt_text(&request),
        ["sloppod: your proposal was approved."]
    );
    let notices: Vec<Value> = h
        .store
        .updates(h.id)
        .into_iter()
        .filter(|u| u["_meta"]["sloppod"]["notice"] == true)
        .collect();
    assert_eq!(notices.len(), 1);
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
}

#[tokio::test]
async fn a_notice_waits_for_a_compaction_to_end() {
    let mut h = Harness::ready().await;
    let turn = h.prompt("First");
    let turn_request = h.next().await;
    let compaction = |status: &str| {
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "agent-session", "update": {
            "sessionUpdate": "compaction_update", "compactionId": "c1", "status": status,
        }}})
    };
    from_agent(&h.hub, h.id, &h.agent, compaction("in_progress")).await;

    h.hub
        .notify(h.id, "sloppod: your proposal was approved.".into())
        .await;
    // Steering it in now would cut the compaction off.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.to_agent.try_recv().is_err());

    from_agent(&h.hub, h.id, &h.agent, compaction("completed")).await;
    let steering = h.next().await;
    assert_eq!(steering["method"], "_session/steering");
    assert_eq!(
        prompt_text(&steering),
        ["sloppod: your proposal was approved."]
    );
    h.respond(&steering, json!({"outcome": "injected"})).await;
    h.respond(&turn_request, json!({"stopReason": "end_turn"}))
        .await;
    turn.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_turn_that_ends_mid_compaction_lets_waiting_steers_go() {
    let mut h = Harness::ready().await;
    let turn = h.prompt("First");
    let turn_request = h.next().await;
    let compacting = json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "agent-session", "update": {
        "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "in_progress",
    }}});
    from_agent(&h.hub, h.id, &h.agent, compacting).await;
    h.hub
        .notify(h.id, "sloppod: your proposal was approved.".into())
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(h.to_agent.try_recv().is_err());

    // The compaction's own end never comes; the turn's does.
    h.respond(&turn_request, json!({"stopReason": "end_turn"}))
        .await;
    turn.await.unwrap().unwrap();
    let steering = h.next().await;
    assert_eq!(steering["method"], "_session/steering");
    h.respond(&steering, json!({"outcome": "promptRequired"}))
        .await;
    let notice_turn = h.next().await;
    assert_eq!(notice_turn["method"], "session/prompt");
    assert_eq!(
        prompt_text(&notice_turn),
        ["sloppod: your proposal was approved."]
    );
    h.respond(&notice_turn, json!({"stopReason": "end_turn"}))
        .await;
}

#[tokio::test]
async fn stopping_a_session_mid_compaction_fails_waiting_steers() {
    let mut h = Harness::ready().await;
    let _turn = h.prompt("First");
    h.next().await;
    let compacting = json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "agent-session", "update": {
        "sessionUpdate": "compaction_update", "compactionId": "c1", "status": "in_progress",
    }}});
    from_agent(&h.hub, h.id, &h.agent, compacting).await;
    let (hub, id) = (h.hub.clone(), h.id);
    let steer = tokio::spawn(async move {
        hub.steer(
            id,
            json!({"sessionId": id, "prompt": [{"type": "text", "text": "Also this"}]}),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!steer.is_finished());

    h.hub.stop(h.id).await;
    let result = tokio::time::timeout(Duration::from_secs(5), steer)
        .await
        .expect("the steer is still waiting")
        .unwrap();
    assert!(result.is_err());
}

#[tokio::test]
async fn a_message_sent_again_does_not_run_twice() {
    let mut h = Harness::ready().await;
    let message_id = Uuid::new_v4();
    let params = json!({"sessionId": h.id, "prompt": [{"type": "text", "text": "Once"}],
        "_meta": {"sloppod": {"messageId": message_id}}});
    let first = {
        let (hub, id, params) = (h.hub.clone(), h.id, params.clone());
        tokio::spawn(async move { hub.prompt(id, params).await })
    };
    let request = h.next().await;
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    first.await.unwrap().unwrap();

    // The UI couldn't tell it arrived, e.g. it only loaded the latest of a long history, and sends it again.
    let again = h.hub.prompt(h.id, params).await.unwrap();
    assert_eq!(again["_meta"]["sloppod"]["alreadyReceived"], true);
    while let Ok(message) = h.to_agent.try_recv() {
        assert!(
            !matches!(message, ToSupervisor::Acp { .. }),
            "the agent was sent {message:?}"
        );
    }
    assert_eq!(h.user_messages(), [("Once".to_owned(), false)]);
}

impl Harness {
    /// Nothing but non-ACP messages (transcript syncs) went to the agent.
    fn agent_sent_nothing(&mut self) {
        while let Ok(message) = self.to_agent.try_recv() {
            assert!(
                !matches!(message, ToSupervisor::Acp { .. }),
                "the agent was sent {message:?}"
            );
        }
    }

    /// The agent's pod went away and a new one came up.
    async fn replace_agent(&mut self) {
        self.hub.detach_agent(self.id, &self.agent).await;
        let (tx, to_agent) = mpsc::unbounded_channel();
        self.agent = Arc::new(AgentConn::new(tx));
        self.to_agent = to_agent;
        self.hub.attach_agent(self.id, self.agent.clone()).await;
        self.agent_ready().await;
    }
}

const STOPPED: &str = "sloppod: your pod was stopped.";

#[tokio::test]
async fn a_retired_agent_gets_no_more_work_and_its_replacement_gets_the_notice() {
    let mut h = Harness::ready().await;
    // Its pod is going away with the agent idle.
    h.hub.retire_agent(h.id, &h.agent).await;
    h.hub.notify(h.id, STOPPED.into()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.agent_sent_nothing();
    assert_eq!(
        h.store.queues().await.unwrap().len(),
        1,
        "kept through the restart"
    );

    h.replace_agent().await;
    assert_eq!(prompt_text(&h.next().await), [STOPPED]);
}

#[tokio::test]
async fn a_turn_that_ends_after_its_agent_retired_hands_nothing_on_to_it() {
    let mut h = Harness::ready().await;
    let turn = h.prompt("Busy");
    let request = h.next().await;
    // The report arrives during the turn, and the turn then ends before the link goes.
    h.hub.retire_agent(h.id, &h.agent).await;
    h.hub.notify(h.id, STOPPED.into()).await;
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    turn.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.agent_sent_nothing();

    h.replace_agent().await;
    assert_eq!(prompt_text(&h.next().await), [STOPPED]);
}

#[tokio::test]
async fn running_commands_are_known_from_the_current_agent_only() {
    let h = Harness::ready().await;
    assert_eq!(
        h.hub.commands(h.id).await,
        None,
        "unknown until the supervisor says"
    );
    h.hub.set_commands(h.id, &h.agent, 2).await;
    assert_eq!(h.hub.commands(h.id).await, Some(2));

    // A new agent starts unknown, and an old one's word no longer counts.
    let (tx, _rx) = mpsc::unbounded_channel();
    let newer = Arc::new(AgentConn::new(tx));
    h.hub.attach_agent(h.id, newer.clone()).await;
    assert_eq!(h.hub.commands(h.id).await, None);
    h.hub.set_commands(h.id, &h.agent, 5).await;
    assert_eq!(h.hub.commands(h.id).await, None);
    h.hub.set_commands(h.id, &newer, 0).await;
    assert_eq!(h.hub.commands(h.id).await, Some(0));
}

#[tokio::test]
async fn a_prompt_during_a_pod_replacement_waits_for_the_new_agent() {
    let mut h = Harness::ready().await;
    // A prompt arrives while the old pod is being deleted.
    let (hub, id) = (h.hub.clone(), h.id);
    let prompt = std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let started = prompt.clone();
    let replaced = h
        .hub
        .replace_pod(h.id, || async move {
            let params =
                json!({"sessionId": id, "prompt": [{"type": "text", "text": "Meanwhile"}]});
            *started.lock().await = Some(tokio::spawn(async move { hub.prompt(id, params).await }));
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok(())
        })
        .await
        .unwrap();
    assert!(replaced);
    h.agent_sent_nothing();

    h.replace_agent().await;
    let request = h.next().await;
    assert_eq!(prompt_text(&request), ["Meanwhile"]);
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    prompt.lock().await.take().unwrap().await.unwrap().unwrap();
}

#[tokio::test]
async fn a_busy_session_keeps_its_pod() {
    let mut h = Harness::ready().await;
    let _turn = h.prompt("Busy");
    h.next().await;
    let replaced = h
        .hub
        .replace_pod(h.id, || async {
            panic!("a busy session's pod was replaced")
        })
        .await
        .unwrap();
    assert!(!replaced);

    // Nor one whose agent has commands running.
    let h = Harness::ready().await;
    h.hub.set_commands(h.id, &h.agent, 1).await;
    let replaced = h
        .hub
        .replace_pod(h.id, || async {
            panic!("a pod with commands running was replaced")
        })
        .await
        .unwrap();
    assert!(!replaced);
}

#[tokio::test]
async fn a_command_run_in_the_pod_gets_its_own_result() {
    let mut h = Harness::ready().await;
    let run = |input: &str| {
        let (agent, input) = (h.agent.clone(), input.to_owned());
        tokio::spawn(async move {
            agent
                .run(
                    vec!["judge".into()],
                    Vec::new(),
                    input,
                    Duration::from_secs(5),
                )
                .await
        })
    };
    let (first, second) = (run("one"), run("two"));
    let mut ids = Vec::new();
    while ids.len() < 2 {
        if let ToSupervisor::Run { id, input, .. } = h.to_agent.recv().await.unwrap() {
            ids.push((id, input));
        }
    }
    ids.sort_by_key(|(_, input)| input.clone());
    // Answered out of order, each gets its own.
    h.agent.run_result(ids[1].0, None, Some("failed".into()));
    h.agent.run_result(ids[0].0, Some("ok".into()), None);
    assert_eq!(first.await.unwrap(), Ok("ok".into()));
    assert_eq!(second.await.unwrap(), Err("failed".into()));

    // A link that goes fails what's still waiting, and what's run after it.
    let pending = run("three");
    tokio::time::sleep(Duration::from_millis(20)).await;
    h.agent.close();
    assert!(pending.await.unwrap().is_err());
    assert!(run("four").await.unwrap().is_err());

    // So does a run whose request can't be sent.
    let (tx, rx) = mpsc::unbounded_channel();
    drop(rx);
    let unsent = AgentConn::new(tx);
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        unsent.run(
            vec!["judge".into()],
            Vec::new(),
            String::new(),
            Duration::from_secs(5),
        ),
    )
    .await
    .expect("a run whose request couldn't be sent waited");
    assert!(result.is_err());
}

#[tokio::test]
async fn a_turn_that_ends_well_is_handed_on_for_checking() {
    let mut h = Harness::ready().await;
    let (tx, mut ended) = mpsc::unbounded_channel();
    h.hub.set_turn_ended(Arc::new(move |id| {
        let tx = tx.clone();
        Box::pin(async move {
            let _ = tx.send(id);
        })
    }));
    let turn = h.prompt("Fine");
    let request = h.next().await;
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    turn.await.unwrap().unwrap();
    let id = tokio::time::timeout(Duration::from_secs(5), ended.recv())
        .await
        .unwrap();
    assert_eq!(id, Some(h.id));

    // One that fails isn't.
    let _failed = h.prompt("Fails");
    let request = h.next().await;
    let error = json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32603, "message": "upstream 503"}});
    from_agent(&h.hub, h.id, &h.agent, error).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(ended.try_recv().is_err());
}

#[tokio::test]
async fn a_conditional_notice_goes_only_to_the_session_as_it_was_seen() {
    let mut h = Harness::ready().await;
    let (seen, running) = h.hub.turn_state(h.id).await;
    assert!(!running);

    // A turn ends after it was seen: the notice is about the past.
    let turn = h.prompt("Meanwhile");
    let request = h.next().await;
    assert!(
        !h.hub
            .notify_when_idle(h.id, Some(seen), "stale".into())
            .await,
        "not while a turn runs"
    );
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    turn.await.unwrap().unwrap();
    assert!(
        !h.hub
            .notify_when_idle(h.id, Some(seen), "stale".into())
            .await
    );
    h.agent_sent_nothing();

    // As it was seen, the notice runs as a turn, and nothing else can start in between.
    let (seen, _) = h.hub.turn_state(h.id).await;
    assert!(
        h.hub
            .notify_when_idle(h.id, Some(seen), "sloppod: start it.".into())
            .await
    );
    assert!(h.hub.turn_state(h.id).await.1, "its turn is reserved");
    let request = h.next().await;
    assert_eq!(prompt_text(&request), ["sloppod: start it."]);
}

#[tokio::test]
async fn a_notice_for_an_idle_session_is_never_steered_into_a_turn() {
    let mut h = Harness::ready().await;
    let turn = h.prompt("Busy");
    let request = h.next().await;
    assert!(
        !h.hub
            .notify_when_idle(h.id, None, "sloppod: a newer image is out.".into())
            .await
    );
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    turn.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    while let Ok(message) = h.to_agent.try_recv() {
        assert!(
            !matches!(message, ToSupervisor::Acp { .. }),
            "the agent was sent {message:?}"
        );
    }
    // Once the session is idle, it goes as a turn of its own.
    assert!(
        h.hub
            .notify_when_idle(h.id, None, "sloppod: a newer image is out.".into())
            .await
    );
    let request = h.next().await;
    assert_eq!(prompt_text(&request), ["sloppod: a newer image is out."]);
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
}

#[tokio::test]
async fn a_notice_waits_for_the_agent_to_be_up() {
    let mut h = Harness::starting().await;
    h.hub
        .notify(h.id, "sloppod: the runtime restarted.".into())
        .await;
    h.agent_ready().await;
    let request = h.next().await;
    assert_eq!(request["method"], "session/prompt");
    assert_eq!(prompt_text(&request), ["sloppod: the runtime restarted."]);
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
}

#[tokio::test]
async fn an_adopted_turn_runs_on_and_ends_as_usual() {
    let turn = json!({"jsonrpc": "2.0", "id": 7, "method": "session/prompt", "params": {"sessionId": "agent-session"}});
    let mut h = Harness::adopted(vec![turn.clone()], Vec::new()).await;
    assert_eq!(h.phase().await, "working");

    let next = h.prompt("Next");
    wait_for_queue(&h, 1).await;
    h.respond(&turn, json!({"stopReason": "end_turn"})).await;
    let request = h.next().await;
    assert_eq!(request["method"], "session/prompt");
    assert_eq!(prompt_text(&request), ["Next"]);
    // Numbered past the request the earlier link left open.
    assert!(request["id"].as_i64().unwrap() > 7);

    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    next.await.unwrap().unwrap();
    assert_eq!(h.phase().await, "idle");
}

#[tokio::test]
async fn an_adopted_question_is_shown_again_and_answered() {
    let question = json!({
        "jsonrpc": "2.0",
        "id": "q-1",
        "method": "elicitation/create",
        "params": {"message": "Which branch?"},
    });
    let turn = json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt", "params": {}});
    let mut h = Harness::adopted(vec![turn], vec![question]).await;
    assert_eq!(h.phase().await, "waiting");
    let replay = h.hub.replay(h.id).await.unwrap();
    assert_eq!(replay.permissions.len(), 1);
    assert_eq!(replay.permissions[0].2["message"], "Which branch?");

    h.hub
        .answer_permission(
            h.id,
            replay.permissions[0].0,
            Ok(json!({"action": "accept"})),
        )
        .await;
    let answer = h.next().await;
    assert_eq!(answer["id"], "q-1");
    assert_eq!(answer["result"]["action"], "accept");
    assert_eq!(h.phase().await, "working");
}

#[tokio::test]
async fn an_idle_adopted_agent_takes_new_prompts() {
    let mut h = Harness::adopted(Vec::new(), Vec::new()).await;
    assert_eq!(h.phase().await, "idle");
    let turn = h.prompt("Hello again");
    let request = h.next().await;
    assert_eq!(request["params"]["sessionId"], "agent-session");
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    turn.await.unwrap().unwrap();
}

#[tokio::test]
async fn parking_waits_for_the_turn_to_end() {
    let mut h = Harness::ready().await;
    let turn = h.prompt("Fix the build");
    let request = h.next().await;
    let suspended = Arc::new(std::sync::Mutex::new(false));
    let parked = h
        .hub
        .park(h.id, || {
            let suspended = suspended.clone();
            async move {
                *suspended.lock().unwrap() = true;
                Ok(())
            }
        })
        .await;
    assert!(matches!(parked, Err(crate::session::ParkError::InTurn)));
    assert!(!*suspended.lock().unwrap());
    assert_eq!(h.phase().await, "working");

    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    turn.await.unwrap().unwrap();
    h.hub.park(h.id, || async { Ok(()) }).await.unwrap();
    assert_eq!(h.phase().await, "parked");
}

#[tokio::test]
async fn a_message_to_a_parked_session_waits_for_its_wake() {
    let h = Harness::ready().await;
    h.hub.park(h.id, || async { Ok(()) }).await.unwrap();
    let turn = h.prompt("Carry on");
    while h.user_messages().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(h.user_messages(), [("Carry on".to_owned(), false)]);
    assert!(!turn.is_finished());
    assert_eq!(h.phase().await, "parked");
}

#[tokio::test]
async fn a_failed_wake_leaves_the_session_parked_to_retry() {
    let h = Harness::ready().await;
    h.hub.park(h.id, || async { Ok(()) }).await.unwrap();

    let failed = h
        .hub
        .wake(h.id, || async {
            Err(anyhow::anyhow!("the API server is down"))
        })
        .await;
    assert!(failed.is_err());
    assert_eq!(h.phase().await, "parked");

    h.hub.wake(h.id, || async { Ok(()) }).await.unwrap();
    assert_eq!(h.phase().await, "starting");
    // Waking a session that isn't parked places nothing.
    h.hub
        .wake(h.id, || async {
            panic!("placed a session that isn't parked")
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn a_wake_during_parking_runs_after_it() {
    let h = Harness::ready().await;
    let steps = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let park = {
        let (hub, id, steps) = (h.hub.clone(), h.id, steps.clone());
        tokio::spawn(async move {
            hub.park(id, || async move {
                released.await.unwrap();
                steps.lock().unwrap().push("suspended");
                Ok(())
            })
            .await
        })
    };
    while h.phase().await != "parked" {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let wake = {
        let (hub, id, steps) = (h.hub.clone(), h.id, steps.clone());
        tokio::spawn(async move {
            hub.wake(id, || async move {
                steps.lock().unwrap().push("placed");
                Ok(())
            })
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(steps.lock().unwrap().is_empty());

    release.send(()).unwrap();
    park.await.unwrap().unwrap();
    wake.await.unwrap().unwrap();
    assert_eq!(*steps.lock().unwrap(), ["suspended", "placed"]);
    assert_eq!(h.phase().await, "starting");
}

#[tokio::test]
async fn a_failed_suspend_gives_the_session_its_agent_back() {
    let mut h = Harness::ready().await;
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let park = {
        let hub = h.hub.clone();
        let id = h.id;
        tokio::spawn(async move {
            hub.park(id, || async move {
                released.await.unwrap();
                Err(anyhow::anyhow!("the API server is down"))
            })
            .await
        })
    };
    while h.phase().await != "parked" {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Sent while parking, so it waits for the agent.
    let turn = h.prompt("Carry on");
    while h.user_messages().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    release.send(()).unwrap();
    assert!(park.await.unwrap().is_err());

    let request = h.next().await;
    assert_eq!(prompt_text(&request), ["Carry on"]);
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    turn.await.unwrap().unwrap();
    assert_eq!(h.phase().await, "idle");
}

#[tokio::test]
async fn a_message_queued_while_parking_wakes_the_session() {
    let h = Harness::ready().await;
    let woken = Arc::new(std::sync::Mutex::new(0));
    {
        let (hub, woken) = (Arc::downgrade(&h.hub), woken.clone());
        h.hub.set_waker(Arc::new(move |id| {
            let (hub, woken) = (hub.clone(), woken.clone());
            Box::pin(async move {
                let hub = hub.upgrade().expect("hub");
                hub.wake(id, || async move {
                    *woken.lock().unwrap() += 1;
                    Ok(())
                })
                .await
            })
        }));
    }
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let park = {
        let (hub, id) = (h.hub.clone(), h.id);
        tokio::spawn(async move {
            hub.park(id, || async move {
                released.await.unwrap();
                Ok(())
            })
            .await
        })
    };
    while h.phase().await != "parked" {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _turn = h.prompt("Carry on");
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The wake waits for the park to finish rather than missing it.
    assert_eq!(*woken.lock().unwrap(), 0);

    release.send(()).unwrap();
    park.await.unwrap().unwrap();
    while *woken.lock().unwrap() == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(h.phase().await, "starting");
}

#[tokio::test]
async fn messages_are_recorded_under_the_ids_their_ui_gave_them() {
    let mut h = Harness::ready().await;
    let (hub, id) = (h.hub.clone(), h.id);
    let prompt = |text: &str, message_id: Uuid| {
        let hub = hub.clone();
        let params = json!({
            "sessionId": id,
            "prompt": [{"type": "text", "text": text}],
            "_meta": {"sloppod": {"messageId": message_id}},
        });
        tokio::spawn(async move { hub.prompt(id, params).await })
    };
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    let turn = prompt("Fix the build", first);
    let request = h.next().await;
    // Sent during the turn, it waits behind it under the same id.
    let queued = prompt("Then the docs", second);
    assert_eq!(wait_for_queue(&h, 1).await, [second.to_string()]);

    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    turn.await.unwrap().unwrap();
    let next = h.next().await;
    h.respond(&next, json!({"stopReason": "end_turn"})).await;
    queued.await.unwrap().unwrap();

    let ids: Vec<String> = h
        .store
        .updates(h.id)
        .iter()
        .filter(|u| u["sessionUpdate"] == "user_message_chunk")
        .map(|u| u["messageId"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(ids, [first.to_string(), second.to_string()]);
}

#[tokio::test]
async fn a_turn_ending_on_a_replaced_agent_leaves_the_new_one_alone() {
    let mut h = Harness::ready().await;
    let old = h.agent.clone();
    let _old_turn = h.prompt("Fix the build");
    h.next().await;

    // The supervisor reconnects: a new agent takes over and starts a turn.
    let (tx, mut to_new) = mpsc::unbounded_channel();
    let new = Arc::new(AgentConn::new(tx));
    h.hub.attach_agent(h.id, new.clone()).await;
    h.hub
        .agent_ready(h.id, &new, "agent-session-2".into(), json!({}), None)
        .await;
    let (hub, id) = (h.hub.clone(), h.id);
    let _new_turn = tokio::spawn(async move {
        let params = json!({"sessionId": id, "prompt": [{"type": "text", "text": "Carry on"}]});
        hub.prompt(id, params).await
    });
    loop {
        match tokio::time::timeout(Duration::from_secs(5), to_new.recv()).await {
            Ok(Some(ToSupervisor::Acp { message })) if message["method"] == "session/prompt" => {
                break;
            }
            Ok(Some(_)) => {}
            other => panic!("the new agent got no prompt: {other:?}"),
        }
    }
    assert_eq!(h.phase().await, "working");

    // The old agent's turn ends late, and changes nothing: the new turn still runs, so the next message waits.
    h.hub.end_turn(h.id, &old).await;
    assert_eq!(h.phase().await, "working");
    let _waiting = h.prompt("And then this");
    assert_eq!(wait_for_queue(&h, 1).await.len(), 1);
}

/// A batch of one message, as `run_queued` gets it, with the receiver of its reply.
fn batch(
    text: &str,
) -> (
    Vec<crate::session::Queued>,
    tokio::sync::oneshot::Receiver<Result<Value, RpcError>>,
) {
    let (reply, receiver) = tokio::sync::oneshot::channel();
    let queued = crate::session::Queued {
        id: None,
        blocks: vec![json!({"type": "text", "text": text})],
        notice: false,
        reply: Some(reply),
    };
    (vec![queued], receiver)
}

/// A new agent replacing the harness's, ready, and the ACP messages it's sent.
async fn replace_agent(h: &Harness) -> (Arc<AgentConn>, mpsc::UnboundedReceiver<ToSupervisor>) {
    let (tx, to_new) = mpsc::unbounded_channel();
    let new = Arc::new(AgentConn::new(tx));
    h.hub.attach_agent(h.id, new.clone()).await;
    h.hub
        .agent_ready(h.id, &new, "agent-session-2".into(), json!({}), None)
        .await;
    (new, to_new)
}

async fn next_prompt(to_agent: &mut mpsc::UnboundedReceiver<ToSupervisor>) -> Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), to_agent.recv()).await {
            Ok(Some(ToSupervisor::Acp { message })) if message["method"] == "session/prompt" => {
                return message;
            }
            Ok(Some(_)) => {}
            other => panic!("no prompt for the agent: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_batch_taken_for_a_replaced_agent_runs_on_the_new_idle_one() {
    let h = Harness::ready().await;
    let old = h.agent.clone();
    let (new, mut to_new) = replace_agent(&h).await;
    let (queued, reply) = batch("Late");
    h.hub
        .clone()
        .run_queued(h.id, old, None, queued, None)
        .await;

    let request = next_prompt(&mut to_new).await;
    assert_eq!(prompt_text(&request), ["Late"]);
    let response =
        json!({"jsonrpc": "2.0", "id": request["id"], "result": {"stopReason": "end_turn"}});
    from_agent(&h.hub, h.id, &new, response).await;
    assert_eq!(reply.await.unwrap().unwrap()["stopReason"], "end_turn");
}

#[tokio::test]
async fn a_batch_for_a_replaced_agent_waits_behind_the_new_ones_turn() {
    let h = Harness::ready().await;
    let old = h.agent.clone();
    let (new, mut to_new) = replace_agent(&h).await;
    let (hub, id) = (h.hub.clone(), h.id);
    let _turn = tokio::spawn(async move {
        hub.prompt(
            id,
            json!({"sessionId": id, "prompt": [{"type": "text", "text": "Busy"}]}),
        )
        .await
    });
    let busy = next_prompt(&mut to_new).await;

    let (queued, reply) = batch("Late");
    h.hub
        .clone()
        .run_queued(h.id, old, None, queued, None)
        .await;
    assert_eq!(h.phase().await, "working");

    // Only once the new agent's turn ends does the batch run on it.
    let response =
        json!({"jsonrpc": "2.0", "id": busy["id"], "result": {"stopReason": "end_turn"}});
    from_agent(&h.hub, h.id, &new, response).await;
    let late = next_prompt(&mut to_new).await;
    assert_eq!(prompt_text(&late), ["Late"]);
    let response =
        json!({"jsonrpc": "2.0", "id": late["id"], "result": {"stopReason": "end_turn"}});
    from_agent(&h.hub, h.id, &new, response).await;
    assert!(reply.await.unwrap().is_ok());
}

#[tokio::test]
async fn the_first_message_is_forgotten_only_once_a_turn_starts_with_it() {
    let mut h = Harness::starting().await;
    h.hub
        .agent_ready(
            h.id,
            &h.agent,
            "agent-session".into(),
            json!({}),
            Some("Hello".into()),
        )
        .await;
    let request = loop {
        let message = h.next().await;
        if message["method"] == "session/prompt" {
            break message;
        }
    };
    assert_eq!(prompt_text(&request), ["Hello"]);
    assert_eq!(*h.store.cleared_prompts.lock().unwrap(), [h.id]);
}

#[tokio::test]
async fn a_first_message_taken_for_a_replaced_agent_stays_stored_for_the_next() {
    let h = Harness::ready().await;
    let old = h.agent.clone();
    let (_new, mut to_new) = replace_agent(&h).await;
    h.hub
        .clone()
        .run_queued(h.id, old, Some("Hello".into()), Vec::new(), None)
        .await;
    // Not run, not forgotten, and not queued in memory either: the next agent reads it from the store. With
    // nothing else to requeue, the new agent isn't sent an empty turn.
    assert!(h.store.cleared_prompts.lock().unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(to_new.try_recv().is_err());
    assert_eq!(h.phase().await, "idle");
}

#[tokio::test]
async fn an_adopted_idle_agent_runs_the_first_message_it_never_got() {
    // The agent came up, then the link dropped before its first turn started: the message is still stored.
    let mut h = Harness::starting().await;
    h.store
        .pending_prompts
        .lock()
        .unwrap()
        .insert(h.id, "Hello".into());
    h.hub
        .adopt_agent(
            h.id,
            &h.agent,
            RunningAgent {
                ready: AgentReady {
                    agent_session_id: "agent-session".into(),
                    capabilities: json!({}),
                },
                open: Vec::new(),
                waiting: Vec::new(),
            },
        )
        .await;
    let request = loop {
        let message = h.next().await;
        if message["method"] == "session/prompt" {
            break message;
        }
    };
    assert_eq!(prompt_text(&request), ["Hello"]);
    assert!(h.store.pending_prompts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_first_message_that_cant_be_marked_taken_isnt_run() {
    let mut h = Harness::starting().await;
    h.store
        .pending_prompts
        .lock()
        .unwrap()
        .insert(h.id, "Hello".into());
    h.store
        .fail_clearing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    h.hub
        .agent_ready(
            h.id,
            &h.agent,
            "agent-session".into(),
            json!({}),
            Some("Hello".into()),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Nothing reached the agent, the message is still stored for the next start, and the session isn't stuck.
    while let Ok(message) = h.to_agent.try_recv() {
        if let ToSupervisor::Acp { message } = message {
            assert_ne!(message["method"], "session/prompt");
        }
    }
    assert_eq!(
        h.store
            .pending_prompts
            .lock()
            .unwrap()
            .get(&h.id)
            .map(String::as_str),
        Some("Hello")
    );
    assert_eq!(h.phase().await, "idle");
}

#[tokio::test]
async fn a_batch_that_can_no_longer_start_in_a_stopped_session_fails() {
    let h = Harness::ready().await;
    let agent = h.agent.clone();
    h.hub.stop(h.id).await;
    let (queued, reply) = batch("Too late");
    h.hub
        .clone()
        .run_queued(h.id, agent, None, queued, None)
        .await;
    assert!(reply.await.unwrap().is_err());
}

#[tokio::test]
async fn a_question_from_ask_user_waits_for_its_answer_without_the_agent() {
    let mut h = Harness::ready().await;
    let questions = json!([{"question": "Which backoff?", "options": [{"label": "Exponential"}, {"label": "Fixed"}]}]);
    h.hub.ask(h.id, questions).await.unwrap();
    // The agent isn't blocked on it, and cancelling a turn leaves it.
    assert_eq!(h.phase().await, "idle");
    h.hub.cancel(h.id).await;
    let replay = h.hub.replay(h.id).await.unwrap();
    let (pid, method, _) = replay.permissions.first().cloned().expect("the question");
    assert_eq!(method, "elicitation/create");

    // Answered, it reaches the agent as a notice.
    let answer = json!({"action": "accept", "content": {"question_0": "Fixed"}});
    h.hub.answer_permission(h.id, pid, Ok(answer)).await;
    // Past the cancel the agent was sent.
    let request = loop {
        let message = h.next().await;
        if message["method"] == "session/prompt" {
            break message;
        }
    };
    assert_eq!(
        prompt_text(&request),
        ["sloppod: the user answered your question:\n- Which backoff?: Fixed"]
    );
    assert!(h.hub.replay(h.id).await.unwrap().permissions.is_empty());
}

#[tokio::test]
async fn a_question_from_ask_user_outlives_a_restart() {
    let h = Harness::ready().await;
    let questions = json!([{"question": "Which backoff?", "options": [{"label": "Exponential"}, {"label": "Fixed"}]}]);
    h.hub.ask(h.id, questions).await.unwrap();

    let (hub, agent, mut to_agent) = restarted(&h).await;
    hub.restore_questions().await;
    let replay = hub.replay(h.id).await.unwrap();
    let (pid, method, params) = replay.permissions.first().cloned().expect("the question");
    assert_eq!(method, "elicitation/create");
    assert_eq!(params["message"], "Which backoff?");

    // Answered after the restart, it reaches the agent and is kept no longer.
    hub.agent_ready(h.id, &agent, "agent-session".into(), json!({}), None)
        .await;
    let answer = json!({"action": "accept", "content": {"question_0": "Fixed"}});
    hub.answer_permission(h.id, pid, Ok(answer)).await;
    assert_eq!(
        prompt_text(&next_prompt(&mut to_agent).await),
        ["sloppod: the user answered your question:\n- Which backoff?: Fixed"]
    );
    assert!(hub.replay(h.id).await.unwrap().permissions.is_empty());
    assert!(h.store.questions().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_queued_message_is_kept_until_its_turn_takes_it() {
    let mut h = Harness::ready().await;
    let _busy = h.prompt("Busy");
    let busy = next_prompt(&mut h.to_agent).await;
    let _later = h.prompt("Later");
    tokio::time::sleep(Duration::from_millis(50)).await;
    let kept = h.store.queues().await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].1[0]["blocks"][0]["text"], "Later");

    h.respond(&busy, json!({"stopReason": "end_turn"})).await;
    let later = next_prompt(&mut h.to_agent).await;
    assert_eq!(prompt_text(&later), ["Later"]);
    // Kept until the agent answers the turn that took it.
    assert_eq!(kept_texts(&h).await, ["Later"]);
    h.respond(&later, json!({"stopReason": "end_turn"})).await;
    wait_until_nothing_kept(&h).await;
}

#[tokio::test]
async fn a_queue_kept_before_a_restart_runs_once_the_agent_is_up() {
    // A restarted runtime: the store has the queue, the hub has nothing yet.
    let mut h = Harness::starting().await;
    h.store
        .save_queue(
            h.id,
            &[json!({"id": null, "blocks": [{"type": "text", "text": "Sent before the restart"}]})],
        )
        .await
        .unwrap();
    h.hub.restore_queues().await;
    h.agent_ready().await;
    let restored = next_prompt(&mut h.to_agent).await;
    assert_eq!(prompt_text(&restored), ["Sent before the restart"]);
    h.respond(&restored, json!({"stopReason": "end_turn"}))
        .await;
    wait_until_nothing_kept(&h).await;
}

#[tokio::test]
async fn a_message_taken_back_before_a_restart_does_not_run_from_the_kept_queue() {
    // Taken back, but the restart came before the queue was saved without it.
    let mut h = Harness::starting().await;
    let (taken, kept) = (Uuid::new_v4(), Uuid::new_v4());
    h.store
        .save_queue(
            h.id,
            &[
                json!({"id": taken, "blocks": [{"type": "text", "text": "Taken back"}]}),
                json!({"id": kept, "blocks": [{"type": "text", "text": "Kept"}]}),
            ],
        )
        .await
        .unwrap();
    h.store.remove_message(h.id, taken).await.unwrap();
    h.hub.restore_queues().await;
    assert_eq!(h.queued().await, [kept.to_string()]);
    h.agent_ready().await;
    let restored = next_prompt(&mut h.to_agent).await;
    assert_eq!(prompt_text(&restored), ["Kept"]);
}

#[tokio::test]
async fn a_message_taken_back_before_a_restart_leaves_nothing_kept() {
    let h = Harness::starting().await;
    let taken = Uuid::new_v4();
    h.store
        .save_queue(
            h.id,
            &[json!({"id": taken, "blocks": [{"type": "text", "text": "Taken back"}]})],
        )
        .await
        .unwrap();
    h.store.remove_message(h.id, taken).await.unwrap();
    h.hub.restore_queues().await;
    assert!(h.queued().await.is_empty());
    assert!(h.store.queues().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_message_steered_before_the_agent_is_up_is_kept() {
    let mut h = Harness::starting().await;
    let steered = h
        .hub
        .clone()
        .steer(
            h.id,
            json!({"sessionId": h.id, "prompt": [{"type": "text", "text": "Steered early"}]}),
        )
        .await
        .unwrap();
    assert_eq!(steered["outcome"], "queued");
    let kept = h.store.queues().await.unwrap();
    assert_eq!(kept[0].1[0]["blocks"][0]["text"], "Steered early");

    h.agent_ready().await;
    assert_eq!(
        prompt_text(&next_prompt(&mut h.to_agent).await),
        ["Steered early"]
    );
}

#[tokio::test]
async fn a_notice_after_a_restore_joins_the_kept_queue() {
    let mut h = Harness::starting().await;
    h.store
        .save_queue(
            h.id,
            &[json!({"id": null, "blocks": [{"type": "text", "text": "Kept"}]})],
        )
        .await
        .unwrap();
    h.hub.restore_queues().await;
    h.hub.clone().notify(h.id, "A notice".into()).await;
    let kept = h.store.queues().await.unwrap();
    let texts: Vec<&str> = kept[0]
        .1
        .iter()
        .filter_map(|m| m["blocks"][0]["text"].as_str())
        .collect();
    assert_eq!(texts, ["Kept", "A notice"]);

    h.agent_ready().await;
    assert_eq!(
        prompt_text(&next_prompt(&mut h.to_agent).await),
        ["Kept", "A notice"]
    );
}

#[tokio::test]
async fn every_turn_that_ends_is_counted_with_its_error() {
    let mut h = Harness::ready().await;
    assert_eq!(h.hub.turns_ended(h.id).await, (0, None));

    let _ok = h.prompt("Fine");
    let request = next_prompt(&mut h.to_agent).await;
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.hub.turns_ended(h.id).await, (1, None));

    // A failed turn is counted too, with its error, however quickly it ends.
    let _failed = h.prompt("Fails");
    let request = next_prompt(&mut h.to_agent).await;
    let error = json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32603, "message": "upstream 503"}});
    from_agent(&h.hub, h.id, &h.agent, error).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        h.hub.turns_ended(h.id).await,
        (2, Some("upstream 503".into()))
    );
}

/// The texts the store keeps for the session's queue.
async fn kept_texts(h: &Harness) -> Vec<String> {
    h.store
        .queues()
        .await
        .unwrap()
        .into_iter()
        .flat_map(|(_, messages)| messages)
        .filter_map(|m| m["blocks"][0]["text"].as_str().map(str::to_owned))
        .collect()
}

/// Waits for the store to keep nothing for the session; a turn lets go of what it carried once its answer is in.
async fn wait_until_nothing_kept(h: &Harness) {
    for _ in 0..100 {
        if h.store.queues().await.unwrap().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("still kept: {:?}", kept_texts(h).await);
}

/// A runtime restart: a new hub on the same store, with what it restores.
async fn restarted(
    h: &Harness,
) -> (
    Arc<Hub>,
    Arc<AgentConn>,
    mpsc::UnboundedReceiver<ToSupervisor>,
) {
    let hub = Arc::new(Hub::new(h.store.clone()));
    hub.set_now(START.parse().unwrap());
    let (tx, to_agent) = mpsc::unbounded_channel();
    let agent = Arc::new(AgentConn::new(tx));
    hub.attach_agent(h.id, agent.clone()).await;
    hub.restore_queues().await;
    (hub, agent, to_agent)
}

/// The agent as it was before the restart, with `open` requests still unanswered.
fn running(open: Vec<Value>) -> RunningAgent {
    RunningAgent {
        ready: AgentReady {
            agent_session_id: "agent-session".into(),
            capabilities: json!({}),
        },
        open,
        waiting: Vec::new(),
    }
}

#[tokio::test]
async fn a_message_taken_back_does_not_run_when_sent_again_after_a_restart() {
    let mut h = Harness::ready().await;
    let _first = h.prompt("First");
    h.next().await;
    let _taken = h.prompt("Taken back");
    let queued = wait_for_queue(&h, 1).await;
    h.hub
        .remove_queued(h.id, json!({"id": queued[0]}))
        .await
        .unwrap();

    // The UI never heard, and sends it again to the restarted runtime.
    let (hub, _agent, _to_agent) = restarted(&h).await;
    let resent = hub
        .prompt(
            h.id,
            json!({"sessionId": h.id, "prompt": [{"type": "text", "text": "Taken back"}],
                "_meta": {"sloppod": {"messageId": queued[0]}}}),
        )
        .await
        .unwrap();
    assert_eq!(resent["stopReason"], "cancelled");
    assert!(h.store.queues().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_notice_is_kept_until_the_agent_answers_its_turn() {
    let mut h = Harness::ready().await;
    h.hub
        .clone()
        .notify(h.id, "sloppod: approved.".into())
        .await;
    assert_eq!(kept_texts(&h).await, ["sloppod: approved."]);
    let turn = next_prompt(&mut h.to_agent).await;
    assert_eq!(prompt_text(&turn), ["sloppod: approved."]);
    // Sent, but only in the runtime's hands until the agent answers.
    assert_eq!(kept_texts(&h).await, ["sloppod: approved."]);
    h.respond(&turn, json!({"stopReason": "end_turn"})).await;
    wait_until_nothing_kept(&h).await;
}

#[tokio::test]
async fn a_notice_a_restart_caught_before_it_was_sent_is_sent_again() {
    let h = Harness::ready().await;
    h.hub
        .clone()
        .notify(h.id, "sloppod: approved.".into())
        .await;
    // The runtime restarts before its turn sends it; the agent comes up anew.
    let (hub, agent, mut to_agent) = restarted(&h).await;
    hub.agent_ready(h.id, &agent, "agent-session".into(), json!({}), None)
        .await;
    assert_eq!(
        prompt_text(&next_prompt(&mut to_agent).await),
        ["sloppod: approved."]
    );
}

#[tokio::test]
async fn a_notice_the_agent_has_isnt_sent_again_after_a_restart() {
    let mut h = Harness::ready().await;
    h.hub
        .clone()
        .notify(h.id, "sloppod: approved.".into())
        .await;
    let turn = next_prompt(&mut h.to_agent).await;
    // The runtime restarts mid-turn; the agent outlived it and still has the turn open.
    let (hub, agent, mut to_agent) = restarted(&h).await;
    hub.adopt_agent(h.id, &agent, running(vec![turn])).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(to_agent.try_recv().is_err(), "not sent twice");
    assert!(h.store.queues().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_notice_sent_but_never_received_is_sent_again_after_a_restart() {
    let mut h = Harness::ready().await;
    h.hub
        .clone()
        .notify(h.id, "sloppod: approved.".into())
        .await;
    let _lost = next_prompt(&mut h.to_agent).await;
    // The restart lost it on the way: the supervisor has nothing open.
    let (hub, agent, mut to_agent) = restarted(&h).await;
    hub.adopt_agent(h.id, &agent, running(Vec::new())).await;
    assert_eq!(
        prompt_text(&next_prompt(&mut to_agent).await),
        ["sloppod: approved."]
    );
}

#[tokio::test]
async fn a_notice_steered_into_a_turn_is_kept_until_the_agent_takes_it() {
    let mut h = Harness::ready().await;
    let turn = h.prompt("Busy");
    let turn_request = next_prompt(&mut h.to_agent).await;
    h.hub
        .clone()
        .notify(h.id, "sloppod: approved.".into())
        .await;
    assert_eq!(kept_texts(&h).await, ["sloppod: approved."]);

    let steering = h.next().await;
    assert_eq!(steering["method"], "_session/steering");
    assert_eq!(
        kept_texts(&h).await,
        ["sloppod: approved."],
        "sent, not yet taken"
    );
    h.respond(&steering, json!({"outcome": "injected"})).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(h.store.queues().await.unwrap().is_empty());

    h.respond(&turn_request, json!({"stopReason": "end_turn"}))
        .await;
    turn.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_notice_whose_steer_failed_after_the_turn_ended_starts_a_turn() {
    let mut h = Harness::ready().await;
    let turn = h.prompt("Busy");
    let turn_request = next_prompt(&mut h.to_agent).await;
    h.hub
        .clone()
        .notify(h.id, "sloppod: approved.".into())
        .await;
    let steering = h.next().await;
    // The turn ends with the steer still open, and then the steer fails.
    h.respond(&turn_request, json!({"stopReason": "end_turn"}))
        .await;
    turn.await.unwrap().unwrap();
    let failed = json!({"jsonrpc": "2.0", "id": steering["id"], "error": {"code": -32603, "message": "boom"}});
    from_agent(&h.hub, h.id, &h.agent, failed).await;
    assert_eq!(
        prompt_text(&next_prompt(&mut h.to_agent).await),
        ["sloppod: approved."]
    );
}

#[tokio::test]
async fn a_steer_a_restart_left_open_and_the_agent_then_refuses_still_delivers_its_notice() {
    let mut h = Harness::ready().await;
    let _turn = h.prompt("Busy");
    let turn_request = next_prompt(&mut h.to_agent).await;
    h.hub
        .clone()
        .notify(h.id, "sloppod: approved.".into())
        .await;
    let steering = h.next().await;
    assert_eq!(steering["method"], "_session/steering");

    // The runtime restarts with both open on the agent.
    let (hub, agent, mut to_agent) = restarted(&h).await;
    hub.adopt_agent(
        h.id,
        &agent,
        running(vec![turn_request.clone(), steering.clone()]),
    )
    .await;
    assert_eq!(
        kept_texts(&h).await,
        ["sloppod: approved."],
        "held until the steer's answer"
    );

    // The turn ends first, so the agent refuses the steer: the notice runs as a turn of its own.
    let answer = |request: &Value, result: Value| json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
    from_agent(
        &hub,
        h.id,
        &agent,
        answer(&turn_request, json!({"stopReason": "end_turn"})),
    )
    .await;
    from_agent(
        &hub,
        h.id,
        &agent,
        answer(&steering, json!({"outcome": "promptRequired"})),
    )
    .await;
    assert_eq!(
        prompt_text(&next_prompt(&mut to_agent).await),
        ["sloppod: approved."]
    );
}

/// `START`, moved on by `minutes`.
fn later(minutes: i64) -> chrono::DateTime<chrono::Utc> {
    START.parse::<chrono::DateTime<chrono::Utc>>().unwrap() + chrono::Duration::minutes(minutes)
}

/// Runs a prompt as a turn that ends at once, and returns what the agent was sent.
async fn prompt_and_answer(h: &mut Harness, text: &str) -> Value {
    let turn = h.prompt(text);
    let request = h.next().await;
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    turn.await.unwrap().unwrap();
    request
}

#[tokio::test]
async fn a_prompt_after_a_gap_tells_the_agent_the_time() {
    let mut h = Harness::ready().await;
    let first = prompt_and_answer(&mut h, "First").await;
    assert_eq!(time_line(&first), None);

    h.hub.set_now(later(20));
    let soon = prompt_and_answer(&mut h, "Soon after").await;
    assert_eq!(time_line(&soon), None, "20 minutes isn't a gap");

    h.hub.set_now(later(20 + 3 * 60 + 5));
    let after_gap = prompt_and_answer(&mut h, "Is CI done?").await;
    assert_eq!(
        all_text(&after_gap),
        [
            "[sloppod: it's 2026-10-27 19:05 UTC, 3 hours since the last message]",
            "Is CI done?"
        ]
    );
    // Only the agent gets the line; the log has the message as it was sent.
    assert_eq!(
        h.user_messages().last().unwrap(),
        &("Is CI done?".to_owned(), false)
    );
}

#[tokio::test]
async fn a_prompt_on_a_new_day_tells_the_agent_the_time() {
    let mut h = Harness::ready().await;
    h.hub.set_now("2026-10-27T23:55:00Z".parse().unwrap());
    prompt_and_answer(&mut h, "Late").await;
    h.hub.set_now("2026-10-28T00:05:00Z".parse().unwrap());
    let request = prompt_and_answer(&mut h, "Early").await;
    assert_eq!(
        time_line(&request).as_deref(),
        Some("[sloppod: it's 2026-10-28 00:05 UTC, 10 minutes since the last message]")
    );
}

#[tokio::test]
async fn a_gap_counts_from_the_agents_last_output_not_the_turns_start() {
    let mut h = Harness::ready().await;
    let turn = h.prompt("Long job");
    let request = h.next().await;
    h.hub.set_now(later(2 * 60));
    let chunk = json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "agent-session", "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."},
    }}});
    from_agent(&h.hub, h.id, &h.agent, chunk).await;
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    turn.await.unwrap().unwrap();

    h.hub.set_now(later(2 * 60 + 10));
    let next = prompt_and_answer(&mut h, "Thanks").await;
    assert_eq!(time_line(&next), None);
}

#[tokio::test]
async fn a_notice_always_carries_the_time() {
    let mut h = Harness::ready().await;
    h.hub.notify(h.id, "sloppod: approved.".into()).await;
    let request = h.next().await;
    assert_eq!(
        all_text(&request),
        ["[sloppod: it's 2026-10-27 15:40 UTC]", "sloppod: approved."]
    );
    h.respond(&request, json!({"stopReason": "end_turn"})).await;

    // Steered into a running turn, too.
    let turn = h.prompt("First");
    let turn_request = h.next().await;
    h.hub.notify(h.id, "sloppod: merged.".into()).await;
    let steering = h.next().await;
    assert_eq!(steering["method"], "_session/steering");
    assert_eq!(
        time_line(&steering).as_deref(),
        Some("[sloppod: it's 2026-10-27 15:40 UTC]")
    );
    let recorded: Vec<Value> = h
        .store
        .updates(h.id)
        .into_iter()
        .filter(|u| u["_meta"]["sloppod"]["notice"] == true)
        .map(|u| u["content"]["text"].clone())
        .collect();
    assert_eq!(recorded, ["sloppod: approved.", "sloppod: merged."]);
    h.respond(&steering, json!({"outcome": "injected"})).await;
    h.respond(&turn_request, json!({"stopReason": "end_turn"}))
        .await;
    turn.await.unwrap().unwrap();
}

#[tokio::test]
async fn after_a_restart_a_gap_counts_from_the_sessions_last_activity() {
    let h = Harness::ready().await;
    let (hub, agent, mut to_agent) = restarted(&h).await;
    hub.set_now(later(2 * 24 * 60 + 30));
    hub.agent_ready(h.id, &agent, "agent-session".into(), json!({}), None)
        .await;
    let (id, prompting) = (h.id, hub.clone());
    tokio::spawn(async move {
        let params = json!({"sessionId": id, "prompt": [{"type": "text", "text": "Back"}]});
        prompting.prompt(id, params).await
    });
    let request = next_prompt(&mut to_agent).await;
    assert_eq!(
        time_line(&request).as_deref(),
        Some("[sloppod: it's 2026-10-29 16:10 UTC, 2 days since the last message]")
    );
}

#[tokio::test]
async fn an_adopted_turn_that_ends_counts_as_hearing_from_the_agent() {
    let turn = json!({"jsonrpc": "2.0", "id": 7, "method": "session/prompt", "params": {"sessionId": "agent-session", "prompt": []}});
    let mut h = Harness::adopted(vec![turn.clone()], Vec::new()).await;
    let chunk = json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "agent-session", "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Working on it."},
    }}});
    from_agent(&h.hub, h.id, &h.agent, chunk).await;
    h.hub.set_now(later(3 * 60));
    let next = h.prompt("Next");
    wait_for_queue(&h, 1).await;
    h.respond(&turn, json!({"stopReason": "end_turn"})).await;
    let request = h.next().await;
    assert_eq!(time_line(&request), None);
    h.respond(&request, json!({"stopReason": "end_turn"})).await;
    next.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_message_kept_across_a_restart_tells_the_agent_how_long_it_waited() {
    let h = Harness::starting().await;
    let (id, hub) = (h.id, h.hub.clone());
    tokio::spawn(async move {
        let params = json!({"sessionId": id, "prompt": [{"type": "text", "text": "Sent while it was down"}]});
        hub.prompt(id, params).await
    });
    for _ in 0..100 {
        if kept_texts(&h).await == ["Sent while it was down"] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The restarted runtime records nothing before the kept message runs.
    let (hub, agent, mut to_agent) = restarted(&h).await;
    hub.set_now(later(3 * 60));
    hub.agent_ready(h.id, &agent, "agent-session".into(), json!({}), None)
        .await;
    let request = next_prompt(&mut to_agent).await;
    assert_eq!(
        all_text(&request),
        [
            "[sloppod: it's 2026-10-27 18:40 UTC, 3 hours since the last message]",
            "Sent while it was down"
        ]
    );
}
