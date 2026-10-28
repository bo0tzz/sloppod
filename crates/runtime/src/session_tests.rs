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
        store.add_session(SessionRow::for_test(id, "dev"));
        let hub = Arc::new(Hub::new(store.clone()));
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

fn prompt_text(request: &Value) -> Vec<String> {
    request["params"]["prompt"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|block| block["text"].as_str().unwrap_or_default().to_owned())
        .collect()
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
    h.hub.clone().run_queued(h.id, old, None, queued).await;

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
    h.hub.clone().run_queued(h.id, old, None, queued).await;
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
        .run_queued(h.id, old, Some("Hello".into()), Vec::new())
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
    h.hub.clone().run_queued(h.id, agent, None, queued).await;
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
