//! The hub against an in-memory store and a scripted agent: what a turn, a queue, a steer and a notice send to the
//! agent and record in the session's log.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use sloppod_protocol::link::ToSupervisor;
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
