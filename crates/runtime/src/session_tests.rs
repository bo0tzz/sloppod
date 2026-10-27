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
