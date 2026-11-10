//! An MCP server's question, the whole way (#481): the session reaches the server through `/servers/<name>`, its
//! agent forwards the question to the user as an ACP elicitation, and the answer goes back to the server.
//!
//! Nothing in the runtime builds an `AppState`, so the hops are driven the way their own code does them: the server
//! the name resolves to comes from `definitions::upstream`, the request is forwarded as the proxy forwards it
//! (`proxy::forward`, through a `Reach` that allows loopback), and the agent's ACP messages enter through
//! `link::from_agent`, as the supervisor's link brings them. What lies in between is real: an HTTP server on loopback,
//! and the hub.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::db::SessionRow;
use crate::definitions::{NewDefinition, grant, upstream};
use crate::link::{AgentConn, from_agent};
use crate::proxy::{Forward, Reach};
use crate::session::Hub;
use crate::store::SessionStore;
use crate::store::memory::MemoryStore;

/// A fake MCP server: the tool call it's given asks the user something before it answers, as a server does. It asks in
/// the call's own body and is answered on a later request, where a real server would use its stream.
#[derive(Clone, Default)]
struct Server {
    /// The tool call it was sent.
    called: Arc<Mutex<Option<Value>>>,
    /// What the session's client answered its question with.
    answered: Arc<Mutex<Option<Value>>>,
}

async fn ask(State(server): State<Server>, body: Bytes) -> impl IntoResponse {
    *server.called.lock().await = Some(serde_json::from_slice(&body).unwrap_or(Value::Null));
    Json(json!({
        "jsonrpc": "2.0",
        "id": "ask-1",
        "method": "elicitation/create",
        "params": {
            "message": "Which environment should I read?",
            "requestedSchema": {"type": "object", "properties": {"environment": {"type": "string"}}},
        },
    }))
}

async fn answered(State(server): State<Server>, body: Bytes) -> impl IntoResponse {
    *server.answered.lock().await = Some(serde_json::from_slice(&body).unwrap_or(Value::Null));
    Json(
        json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": "read"}]}}),
    )
}

/// A session with an agent attached and ready, in a memory store, and what the hub sends that agent.
async fn session() -> (
    Arc<Hub>,
    Arc<MemoryStore>,
    Uuid,
    Arc<AgentConn>,
    mpsc::UnboundedReceiver<sloppod_protocol::link::ToSupervisor>,
) {
    let store = Arc::new(MemoryStore::default());
    let id = Uuid::new_v4();
    store.add_session(SessionRow::for_test(id, "dev"));
    let hub = Arc::new(Hub::new(store.clone()));
    let (to_agent, sent) = mpsc::unbounded_channel();
    let agent = Arc::new(AgentConn::new(to_agent));
    hub.attach_agent(id, agent.clone()).await;
    hub.agent_ready(id, &agent, "agent-session".into(), json!({}), None)
        .await;
    (hub, store, id, agent, sent)
}

/// Sends one request to `url` as the runtime's proxy sends a session's, and gives back what came of it.
async fn call(reach: &Reach, url: &str, body: Value) -> Value {
    let response = reach
        .forward(Forward {
            method: Method::POST,
            headers: &HeaderMap::new(),
            url,
            body: Bytes::from(serde_json::to_vec(&body).unwrap()),
            set: Vec::new(),
            remove: vec!["authorization".into(), "cookie".into()],
            user_header: "x-sloppod-user",
        })
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn an_mcp_servers_question_reaches_the_user_and_its_answer_goes_back(db: sqlx::PgPool) {
    // The owner's server, listening on loopback.
    let server = Server::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/mcp", post(ask))
        .route("/answered", post(answered))
        .with_state(server.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let definition = crate::definitions::create(
        &db,
        "dev",
        &NewDefinition {
            name: "logs".into(),
            description: "Production logs".into(),
            mcp_url: format!("http://{addr}/mcp"),
            credential_id: None,
        },
    )
    .await
    .unwrap()
    .expect("the definition");
    // What `/servers/logs` resolves a session's request to, grant and all.
    let resolved = upstream(&db, "dev", &[grant(&definition)], "logs")
        .await
        .unwrap()
        .expect("the session holds the grant");
    assert_eq!(resolved.mcp_url, definition.mcp_url);

    // The session and its agent, as the link presents them.
    let (hub, store, id, agent, mut sent) = session().await;

    // The session's client calls the server's tool, through the proxy.
    let reach = Reach::local(Duration::from_secs(10));
    let asked = call(
        &reach,
        &resolved.mcp_url,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "read", "arguments": {}}}),
    )
    .await;
    assert_eq!(asked["method"], "elicitation/create", "{asked}");
    assert_eq!(
        server.called.lock().await.clone().unwrap()["params"]["name"],
        "read"
    );

    // Its agent reports the call it belongs to, and forwards the question as an ACP elicitation, as the adapters do.
    from_agent(
        &hub,
        id,
        &agent,
        json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": id, "update": {
            "sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "mcp__logs__read",
            "status": "in_progress", "rawInput": {"server": "logs", "tool": "read", "arguments": {}},
        }}}),
    )
    .await;
    from_agent(
        &hub,
        id,
        &agent,
        json!({"jsonrpc": "2.0", "id": 7, "method": "elicitation/create", "params": {
            "sessionId": id,
            "mode": "form",
            "message": asked["params"]["message"],
            "requestedSchema": asked["params"]["requestedSchema"],
        }}),
    )
    .await;

    // The user is asked, in the same place an `ask_user` question puts them, and told which server is asking — which
    // the runtime works out from the call that is running.
    let row = store.get_session(id).await.unwrap().unwrap();
    assert_eq!(hub.session_api(&row).await.waiting.questions, 1);
    let replay = hub.replay(id).await.unwrap();
    let (pid, method, params) = replay
        .permissions
        .first()
        .cloned()
        .expect("the question reached the user");
    assert_eq!(method, "elicitation/create");
    assert_eq!(params["_meta"]["sloppod"]["server"], "logs", "{params}");
    assert_eq!(params["message"], "Which environment should I read?");
    assert!(
        params["_meta"]["sloppod"]["cardId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "no id of the request's own: {params}"
    );

    // Answered, it reaches the agent: the call it was holding can go on.
    let answer = json!({"action": "accept", "content": {"environment": "production"}});
    hub.answer_permission(id, pid, Ok(answer.clone()), "a test")
        .await;
    assert_eq!(hub.session_api(&row).await.waiting.questions, 0);
    let response = loop {
        match sent.recv().await.expect("the link closed") {
            sloppod_protocol::link::ToSupervisor::Acp { message } => {
                if message["id"] == json!(7) {
                    break message;
                }
            }
            _ => continue,
        }
    };
    assert_eq!(response["result"], answer);

    // And what the agent answers the server with, through the proxy, is what the user chose.
    let result = call(
        &reach,
        &format!("http://{addr}/answered"),
        json!({"jsonrpc": "2.0", "id": "ask-1", "result": response["result"]}),
    )
    .await;
    assert_eq!(result["result"]["content"][0]["text"], "read");
    assert_eq!(
        server.answered.lock().await.clone().unwrap()["result"]["content"]["environment"],
        "production"
    );
}

#[tokio::test]
async fn a_request_carries_an_id_of_its_own_whatever_the_server_calls_it() {
    let (hub, _store, id, agent, _sent) = session().await;

    // Two servers' URL elicitations, each free to choose its own id: both say "login" (#481).
    for request_id in [7, 8] {
        from_agent(
            &hub,
            id,
            &agent,
            json!({"jsonrpc": "2.0", "id": request_id, "method": "elicitation/create", "params": {
                "sessionId": id,
                "mode": "url",
                "message": "Authorize me",
                "url": "https://logs.example.com/oauth/authorize",
                "elicitationId": "login",
            }}),
        )
        .await;
    }

    let requests = |replay: &crate::session::Replay| {
        replay
            .permissions
            .iter()
            .map(|(_, _, params)| params.clone())
            .collect::<Vec<Value>>()
    };
    let asked = requests(&hub.replay(id).await.unwrap());
    assert_eq!(asked.len(), 2, "{asked:?}");
    // Each carries an id of sloppod's own, so the UIs hold both rather than merging them into one.
    let cards: Vec<&str> = asked
        .iter()
        .map(|params| {
            params["_meta"]["sloppod"]["cardId"]
                .as_str()
                .unwrap_or_default()
        })
        .collect();
    assert!(cards.iter().all(|card| !card.is_empty()), "{asked:?}");
    assert_ne!(cards[0], cards[1], "{asked:?}");
    // The server's own id is left as it was: its answer goes back to it under it.
    assert!(
        asked
            .iter()
            .all(|params| params["elicitationId"] == "login")
    );
    // And a UI that loads the session again is sent the requests as they are, ids and all.
    assert_eq!(requests(&hub.replay(id).await.unwrap()), asked);
}
