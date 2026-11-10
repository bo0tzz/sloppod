//! The runtime's MCP endpoint: tools for proposals, reached from session pods and identified by pod IP.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use sloppod_contracts::SessionRole;
use sloppod_protocol::api::{ProposalStatus, SessionKind};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{AppState, db, proposals, reviews, subsessions, work};

/// Every protocol version the endpoint serves, newest first. The first is the stateless era (2026-07-28: every request
/// carries its version and client capabilities in `_meta`); the others are the era of `initialize`.
/// The versions sloppod serves, newest first: `[0]` is the stateless one, the rest are served with the handshake.
/// `2024-11-05` is still offered by clients, and its requests are the same 2025 shape ones. A handshake client asking
/// for a version that isn't here is answered with `[1]` — the newest of that era — rather than refused, which is what
/// the 2025 spec's lifecycle says a server does, and the client decides whether it can go on with it.
const SUPPORTED: [&str; 5] = [
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
const STATELESS: &str = "2026-07-28";
const DEFAULT_LEGACY: &str = "2025-06-18";

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
const META_SUBSCRIPTION: &str = "io.modelcontextprotocol/subscriptionId";

const INSTRUCTIONS: &str = "sloppod's tools for this session: proposals of outward actions (a person reviews them before sloppod carries them out), questions to the user, what the session is granted, and, for work sessions, sub-sessions. Each tool says what it does.";

/// What the protocol layer needs of the session on the other end: its tools and the tool-list changes of #323. The
/// runtime implements it over the database; tests, over a list they set.
pub trait Session: Sync {
    /// The build, for `serverInfo`.
    fn version(&self) -> &str;
    /// The tools the session has now; an error says why it has none.
    fn tools(&self) -> impl Future<Output = Result<Vec<Value>, String>> + Send;
    /// Runs a `tools/call`'s params and returns its result.
    fn call(&self, params: &Value) -> impl Future<Output = Value> + Send;
    /// Notes the list the client was given.
    fn listed(&self, tools: &[Value]);
    fn subscribe(&self) -> broadcast::Receiver<()>;
    /// Whether the client was given a list other than `tools`.
    fn stale(&self, tools: &[Value]) -> bool;
}

struct RuntimeSession<'a> {
    state: &'a AppState,
    id: Uuid,
}

impl Session for RuntimeSession<'_> {
    fn version(&self) -> &str {
        self.state
            .config
            .build
            .as_deref()
            .unwrap_or(env!("CARGO_PKG_VERSION"))
    }

    async fn tools(&self) -> Result<Vec<Value>, String> {
        let row = session_kind(self.state, self.id).await?;
        Ok(tools(self.state, &row).await)
    }

    async fn call(&self, params: &Value) -> Value {
        call(self.state, self.id, params).await
    }

    fn listed(&self, tools: &[Value]) {
        self.state.tool_streams.listed(self.id, tools);
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.state.tool_streams.subscribe(self.id)
    }

    fn stale(&self, tools: &[Value]) -> bool {
        self.state.tool_streams.stale(self.id, tools)
    }
}

type Messages = Pin<Box<dyn Stream<Item = Value> + Send>>;

/// What a POST to `/mcp` is answered with.
pub enum Outcome {
    /// A notification, which gets no reply.
    Accepted,
    Reply(StatusCode, Value),
    /// The messages of a response that stays open.
    Listen(Messages),
}

fn rpc(id: Value, result: Result<Value, Value>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
    }
}

fn error(code: i64, message: impl Into<String>) -> Value {
    json!({"code": code, "message": message.into()})
}

fn unsupported_version(requested: &str) -> Value {
    json!({
        "code": -32022,
        "message": "Unsupported protocol version",
        "data": {"supported": SUPPORTED, "requested": requested},
    })
}

fn server_info(session: &impl Session) -> Value {
    json!({"name": "sloppod", "version": session.version()})
}

/// A result of the stateless era: it says it's complete, and who answered.
fn complete(session: &impl Session, mut result: Value) -> Value {
    result["resultType"] = json!("complete");
    result["_meta"][META_SERVER_INFO] = server_info(session);
    result
}

/// Whether a request is of the stateless era: its `_meta` has the era's keys, or its header names the era, which
/// with no `_meta` is then an error. Any other request is of the era of `initialize`, whose later requests carry
/// neither.
fn stateless(params: &Value, headers: &Headers<'_>) -> bool {
    let meta = &params["_meta"];
    [META_VERSION, META_CAPABILITIES, META_CLIENT_INFO]
        .iter()
        .any(|key| meta.get(key).is_some())
        || headers.version == Some(STATELESS)
}

/// The standard headers of a POST to `/mcp` that the stateless era mirrors from the body, for the server to check
/// against it: what a proxy routing on a header sees must be what the server runs.
#[derive(Default, Clone, Copy)]
pub struct Headers<'a> {
    /// `MCP-Protocol-Version`
    pub version: Option<&'a str>,
    /// `Mcp-Method`
    pub method: Option<&'a str>,
    /// `Mcp-Name`, possibly in its Base64 sentinel form
    pub name: Option<&'a str>,
}

impl Headers<'_> {
    /// Says what's missing or doesn't match the body, as `HeaderMismatch` words it.
    ///
    /// What the real clients send, when this was written (Claude Code 2.1.310 and Codex 0.164.0, run against a
    /// logging MCP server): Claude probes `server/discover` first and, once that's answered, sends `mcp-method`,
    /// `mcp-protocol-version` and — for a `tools/call` — `mcp-name`, so it meets these checks; Codex starts with a
    /// 2025 `initialize` and stays in that era, whose requests it sends with `mcp-protocol-version` and no
    /// `Mcp-Method`. Neither sends these headers on `initialize` or on a 2025 request, and none of them is asked for
    /// one here: a request is only validated when it's of the stateless era.
    fn validate(&self, method: &str, params: &Value) -> Result<(), String> {
        let version = params["_meta"][META_VERSION].as_str().unwrap_or_default();
        match self.version {
            None => return Err("Header mismatch: MCP-Protocol-Version header is missing".into()),
            Some(header) if header != version => {
                return Err(format!(
                    "Header mismatch: MCP-Protocol-Version header value '{header}' does not match body value '{version}'"
                ));
            }
            Some(_) => {}
        }
        match self.method {
            None => return Err("Header mismatch: Mcp-Method header is missing".into()),
            Some(header) if header != method => {
                return Err(format!(
                    "Header mismatch: Mcp-Method header value '{header}' does not match body value '{method}'"
                ));
            }
            Some(_) => {}
        }
        if method == "tools/call" {
            let name = params["name"].as_str().unwrap_or_default();
            let Some(header) = self.name else {
                return Err("Header mismatch: Mcp-Name header is missing".into());
            };
            let decoded = decode_name(header)
                .ok_or("Header mismatch: Mcp-Name header is not valid Base64 of UTF-8")?;
            if decoded != name {
                return Err(format!(
                    "Header mismatch: Mcp-Name header value '{decoded}' does not match body value '{name}'"
                ));
            }
        }
        Ok(())
    }
}

/// A header value that may carry the `=?base64?...?=` sentinel for a name that isn't plain ASCII.
fn decode_name(header: &str) -> Option<String> {
    use base64::Engine;
    match header
        .strip_prefix("=?base64?")
        .and_then(|rest| rest.strip_suffix("?="))
    {
        Some(encoded) => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()?;
            String::from_utf8(bytes).ok()
        }
        None => Some(header.to_owned()),
    }
}

/// Answers one JSON-RPC message of a POST to `/mcp`.
pub async fn serve(session: &impl Session, message: Value, headers: &Headers<'_>) -> Outcome {
    let Some(id) = message.get("id").cloned() else {
        return Outcome::Accepted;
    };
    let method = message["method"].as_str().unwrap_or_default();
    let params = &message["params"];
    if method != "initialize" && stateless(params, headers) {
        return serve_stateless(session, id, method, params, headers).await;
    }
    let result = match method {
        "initialize" => {
            // A version it doesn't serve gets its newest of the era instead, and the client decides whether it can go on.
            let requested = params["protocolVersion"].as_str().unwrap_or(DEFAULT_LEGACY);
            let version = SUPPORTED[1..]
                .iter()
                .find(|v| **v == requested)
                .unwrap_or(&SUPPORTED[1]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": server_info(session),
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => list_tools(session).await,
        "tools/call" => Ok(session.call(params).await),
        _ => Err(error(-32601, format!("{method} is not supported"))),
    };
    Outcome::Reply(StatusCode::OK, rpc(id, result))
}

async fn list_tools(session: &impl Session) -> Result<Value, Value> {
    match session.tools().await {
        Ok(tools) => {
            session.listed(&tools);
            Ok(json!({"tools": tools}))
        }
        Err(e) => Err(error(-32603, e)),
    }
}

async fn serve_stateless(
    session: &impl Session,
    id: Value,
    method: &str,
    params: &Value,
    headers: &Headers<'_>,
) -> Outcome {
    let meta = &params["_meta"];
    let refuse = |status, error| Outcome::Reply(status, rpc(id.clone(), Err(error)));
    match meta[META_VERSION].as_str() {
        Some(STATELESS) => {}
        Some(other) => {
            return refuse(StatusCode::BAD_REQUEST, unsupported_version(other));
        }
        None => {
            return refuse(
                StatusCode::BAD_REQUEST,
                error(-32602, format!("_meta.{META_VERSION} is required")),
            );
        }
    }
    if !meta[META_CAPABILITIES].is_object() {
        return refuse(
            StatusCode::BAD_REQUEST,
            error(-32602, format!("_meta.{META_CAPABILITIES} is required")),
        );
    }
    if let Err(mismatch) = headers.validate(method, params) {
        return refuse(StatusCode::BAD_REQUEST, error(-32020, mismatch));
    }
    let result = match method {
        "server/discover" => Ok(json!({
            "supportedVersions": SUPPORTED,
            "capabilities": {"tools": {"listChanged": true}},
            "instructions": INSTRUCTIONS,
        })),
        "tools/list" => list_tools(session).await,
        "tools/call" => Ok(session.call(params).await),
        "subscriptions/listen" => return Outcome::Listen(listen(session, id, params).await),
        _ => {
            return refuse(
                StatusCode::NOT_FOUND,
                error(-32601, format!("{method} is not supported")),
            );
        }
    };
    Outcome::Reply(
        StatusCode::OK,
        rpc(id, result.map(|result| complete(session, result))),
    )
}

/// `subscriptions/listen`: acknowledges, then sends what the request asked to hear of, each tagged with the request's
/// id. Only tool-list changes exist here.
async fn listen(session: &impl Session, id: Value, params: &Value) -> Messages {
    let tools_changed = params["notifications"]["toolsListChanged"] == json!(true);
    let granted = if tools_changed {
        json!({"toolsListChanged": true})
    } else {
        json!({})
    };
    let acknowledged = json!({
        "jsonrpc": "2.0",
        "method": "notifications/subscriptions/acknowledged",
        "params": {"notifications": granted},
    });
    let notices = if tools_changed {
        tool_notices(session).await
    } else {
        Box::pin(futures::stream::pending())
    };
    let messages = futures::stream::once(std::future::ready(acknowledged)).chain(notices);
    Box::pin(messages.map(move |mut message| {
        message["params"]["_meta"][META_SUBSCRIPTION] = id.clone();
        message
    }))
}

/// The `notifications/tools/list_changed` for the session's client: one at once if it missed a change, and one for
/// each after.
async fn tool_notices(session: &impl Session) -> Messages {
    // Subscribed before it's checked, so a change in between is either sent on the stream or found stale here.
    let changed = session.subscribe();
    let missed = session.stale(&session.tools().await.unwrap_or_default());
    let notice = || json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
    let notices = futures::stream::unfold(changed, move |mut changed| async move {
        // A lagged receiver missed only repeats of the same notice.
        if let Err(broadcast::error::RecvError::Closed) = changed.recv().await {
            return None;
        }
        Some((notice(), changed))
    });
    Box::pin(futures::stream::iter(missed.then(notice)).chain(notices))
}

fn sse(messages: Messages) -> Response {
    let events = messages.map(|message| {
        Ok::<_, std::convert::Infallible>(
            Event::default().event("message").data(message.to_string()),
        )
    });
    Sse::new(events)
        .keep_alive(KeepAlive::default())
        .into_response()
}

pub async fn handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(message): Json<Value>,
) -> Response {
    let Some(id) = crate::identity::calling_session(&state, &headers, peer.ip()).await else {
        return (StatusCode::FORBIDDEN, "not a session pod").into_response();
    };
    let header = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let headers = Headers {
        version: header("mcp-protocol-version"),
        method: header("mcp-method"),
        name: header("mcp-name"),
    };
    let session = RuntimeSession { state: &state, id };
    match serve(&session, message, &headers).await {
        Outcome::Accepted => StatusCode::ACCEPTED.into_response(),
        Outcome::Reply(status, body) => (status, Json(body)).into_response(),
        Outcome::Listen(messages) => sse(messages),
    }
}

/// What each session's agent was last told its tools are, and the streams it listens on for a change to them (#323).
/// An agent's MCP client fetches the tool list when it connects; a grant that changes the list mid-session would
/// otherwise reach it only at its next start.
#[derive(Default)]
pub struct ToolStreams(StdMutex<HashMap<Uuid, Listed>>);

struct Listed {
    /// A digest of the tool list the session was last given, if it asked.
    tools: Option<u64>,
    changed: broadcast::Sender<()>,
}

impl Listed {
    fn new() -> Self {
        Self {
            tools: None,
            changed: broadcast::channel(4).0,
        }
    }
}

fn digest(tools: &[Value]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    Value::Array(tools.to_vec()).to_string().hash(&mut hasher);
    hasher.finish()
}

impl ToolStreams {
    fn listed(&self, session: Uuid, tools: &[Value]) {
        let mut sessions = self.0.lock().unwrap();
        sessions.entry(session).or_insert_with(Listed::new).tools = Some(digest(tools));
    }

    fn subscribe(&self, session: Uuid) -> broadcast::Receiver<()> {
        let mut sessions = self.0.lock().unwrap();
        sessions
            .entry(session)
            .or_insert_with(Listed::new)
            .changed
            .subscribe()
    }

    /// Whether the session's client was given a list other than `tools`: a change it missed while it had no stream
    /// open, which it hears of when it opens one.
    fn stale(&self, session: Uuid, tools: &[Value]) -> bool {
        let sessions = self.0.lock().unwrap();
        sessions
            .get(&session)
            .and_then(|listed| listed.tools)
            .is_some_and(|given| given != digest(tools))
    }

    /// Forgets a session that's gone.
    pub fn forget(&self, session: Uuid) {
        self.0.lock().unwrap().remove(&session);
    }

    /// Tells the session's agent its tools changed, if they differ from what it was last given. Says whether it did.
    fn changed(&self, session: Uuid, tools: &[Value]) -> bool {
        let mut sessions = self.0.lock().unwrap();
        let Some(listed) = sessions.get_mut(&session) else {
            return false;
        };
        let now = Some(digest(tools));
        if listed.tools.is_none() || listed.tools == now {
            return false;
        }
        // An agent with no stream open doesn't hear of it, and still has the list it was given.
        let sent = listed.changed.send(()).is_ok();
        if sent {
            listed.tools = now;
        }
        sent
    }
}

/// Sends `notifications/tools/list_changed` to the session's agent when its tools are no longer the ones it was
/// given, e.g. after a grant change (#323). Its client then lists them again.
pub async fn tools_changed(state: &AppState, session: Uuid) {
    let tools = current_tools(state, session).await;
    if state.tool_streams.changed(session, &tools) {
        tracing::info!(%session, "told the agent its tools changed");
    }
}

/// The tools the session has now; none if it can't act.
async fn current_tools(state: &AppState, session: Uuid) -> Vec<Value> {
    match session_kind(state, session).await {
        Ok(row) => tools(state, &row).await,
        Err(_) => Vec::new(),
    }
}

/// `GET /mcp`: the stream the server sends the agent's MCP client notifications on (MCP's streamable HTTP until
/// 2026-07-28, whose clients listen with `subscriptions/listen` instead). The only one sent is
/// `notifications/tools/list_changed`.
pub async fn stream(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(id) = crate::identity::calling_session(&state, &headers, peer.ip()).await else {
        return (StatusCode::FORBIDDEN, "not a session pod").into_response();
    };
    sse(tool_notices(&RuntimeSession { state: &state, id }).await)
}

/// Which tools a session gets depends on its kind, so a session that can't be loaded gets none.
async fn session_kind(state: &AppState, session: Uuid) -> Result<db::SessionRow, String> {
    match db::get_session(&state.db, session).await {
        Ok(Some(row)) => match row.not_acting() {
            None => Ok(row),
            Some(why) => Err(format!("this session is {why}, so it has no tools")),
        },
        Ok(None) => Err("this session no longer exists".into()),
        Err(e) => Err(format!("couldn't load the session: {e}")),
    }
}

const LIST_GRANTS: &str = "list_grants";

fn list_grants() -> Value {
    json!({
        "name": LIST_GRANTS,
        "description": "List what this session is granted: the repositories it can read and push to, the APIs it can call and the hosts it can reach.",
        "inputSchema": {"type": "object", "properties": {}},
    })
}

/// A review session's tools (ADR 24): it reports its verdict, and proposes nothing.
fn review_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "submit_review",
            "description": "Submit your review of the change you were asked to review. This is how each round ends; nobody reads chat replies.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "verdict": {"type": "string", "enum": ["pass", "changes"], "description": "`changes` when the author should fix something before a human looks, `pass` otherwise; never `pass` with a p0"},
                    "summary": {"type": "string", "description": "Two to four sentences on what the change does and how, and what merging it sets off where you read evidence of it. No verdict, and not a list of the findings"},
                    "findings": {
                        "type": "array",
                        "description": "What should change, one point each; required with `changes`, empty when nothing is worth flagging",
                        "items": {
                            "type": "object",
                            "properties": {
                                "severity": {"type": "string", "enum": ["p0", "p1", "p2"], "description": "p0: must be fixed before this merges. p1: should be fixed. p2: optional polish"},
                                "category": {"type": "string", "enum": ["correctness", "security", "performance", "reliability", "maintainability", "tests"], "description": "What the fix is really about. correctness: wrong behaviour or a broken contract. security: exposure, injection, secrets, unsafe defaults, data loss. performance: cost in time, memory or calls. reliability: error handling, retries, timeouts, concurrency, leaks. maintainability: structure, naming, clarity, stale docs. tests: a test the change needs or one that's wrong"},
                                "title": {"type": "string", "description": "One line, under 80 characters"},
                                "explanation": {"type": "string", "description": "Under what inputs or state it bites, and why it matters"},
                                "path": {"type": "string", "description": "File it's about, relative to the repository root"},
                                "line": {"type": "integer", "description": "Line in the file at the reviewed commit"},
                                "end_line": {"type": "integer", "description": "Last line of the range, when it covers more than one"},
                                "suggested_fix": {"type": "string", "description": "The concrete change to make"},
                                "prior": {"type": "string", "description": "In a later round: the id of the earlier finding this one reports again. Omit for a new finding"},
                            },
                            "required": ["severity", "category", "title", "explanation"],
                        },
                    },
                    "resolved": {"type": "array", "items": {"type": "string"}, "description": "In a later round: ids of the earlier findings that no longer hold"},
                },
                "required": ["verdict", "summary"],
            },
        }),
        list_grants(),
    ]
}

const HIGHLIGHT: &str = "highlight";
const RETRACT_HIGHLIGHT: &str = "retract_highlight";
const WITHDRAW_QUESTION: &str = "withdraw_question";

/// Whether a session has `highlight`: only one the user talks to, so not a sub-session, whose report reaches its
/// parent (ADR 27), nor a reviewer.
fn highlight_offered(role: &SessionRole) -> bool {
    matches!(role, SessionRole::Work)
}

async fn tools(state: &AppState, row: &db::SessionRow) -> Vec<Value> {
    let kind = row.kind;
    let context = row.context();
    match kind {
        SessionKind::Review => return review_tools(),
        SessionKind::Utility => return Vec::new(),
        SessionKind::Work => {}
    }
    let mut tools: Vec<Value> = state
        .proposals
        .tools()
        .into_iter()
        .filter(|t| work::agent::shown(&context, &t.name))
        .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema}))
        .collect();
    tools.push(list_grants());
    tools.push(json!({
        "name": "get_proposal",
        "description": "Check the status of a proposal you made: in_review (another model is reviewing it), changes_requested (by that review), pending (waiting for a human), approved, rejected (with the reviewer's reason), withdrawn, executed (with the result, e.g. the PR URL) or failed.",
        "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
    }));
    tools.push(json!({
        "name": "withdraw_proposal",
        "description": "Withdraw a proposal of yours that hasn't been decided yet, e.g. one that's wrong or that you've replaced. Nothing it proposed is carried out, and it's no longer shown to the reviewer.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "reason": {"type": "string", "description": "Why, kept with the proposal"},
            },
            "required": ["id"],
        },
    }));
    tools.push(json!({
        "name": "ask_user",
        "description": "Ask the user one to four multiple-choice questions without waiting for them: the questions are shown in their client, this returns at once, and their answer arrives later as a message starting \"sloppod: the user answered your question\", or \"sloppod: the user dismissed your question without answering it\" if they close it. An open question survives a runtime restart. Carry on with work that doesn't depend on it meanwhile.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string", "description": "The question, ending with a question mark"},
                            "header": {"type": "string", "description": "A short label for it, e.g. \"Auth method\""},
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string", "description": "What choosing it means"},
                                    },
                                    "required": ["label"],
                                },
                            },
                            "multi_select": {"type": "boolean", "default": false},
                        },
                        "required": ["question", "options"],
                    },
                },
            },
            "required": ["questions"],
        },
    }));
    // Only a session the user talks to: a sub-session's report goes to its parent, which highlights what they need
    // (ADR 27).
    if highlight_offered(&row.role()) {
        tools.push(highlight_tool());
        tools.push(retract_highlight_tool());
    }
    tools.push(withdraw_question_tool());
    tools.push(json!({
        "name": "request_review",
        "description": "After a review asked for changes to your proposal or to a branch you asked `request_code_review` for, ask for the next round once you've pushed fixes. The proposal is re-pinned to your branch's new head.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "update": {"type": "object", "description": "A new title or description when they no longer describe the change: {title?, body?} for a pull request, {pull_requests: [{title?, body?}]} by position for a stack, {title?, summary?} for a report, {focus} for a code review. What's proposed (repository, branches) can't change; withdraw and propose again for that"},
                "replies": {"type": "string", "description": "Your replies to findings you think don't apply, or that you addressed differently"},
            },
            "required": ["id"],
        },
    }));
    if kind == SessionKind::Work {
        // The user's endpoint models a child can run on (#461), by the names `spawn_session` takes.
        let models: Vec<String> = state
            .providers
            .offered(&row.owner)
            .await
            .into_iter()
            .map(|m| m.name)
            .collect();
        tools.extend(subsessions::tools(&models));
    }
    tools.extend(work::agent::tools(&context));
    tools
}

/// What the agent may highlight at most, in characters: a highlight is read in a list, not as a document.
const HIGHLIGHT_LIMIT: usize = 2000;

fn highlight_tool() -> Value {
    json!({
        "name": HIGHLIGHT,
        "description": "Put something in front of the user that they'd want to find again without reading the whole conversation: the answer to what they asked, something they have to do that isn't a decision, or a serious finding they didn't ask about. It shows in the chat where you call it, as a highlighted card like a message of yours, so don't also write its content in a reply: a reply around it adds only what isn't in it. It's also in the session's \"for you\" list, where each stays until they mark it seen, and the session counts as waiting on them meanwhile. Marking one seen starts no turn; you hear which of your highlights were marked seen with your next turn. Read out of context: say what it answers, lead with the conclusion, keep it to a few lines, link what it refers to. Highlight each thing once; if it changes, highlight the new state. Not for progress, plans, or what a proposal or a question already shows.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "What the user should see, in Markdown, a few lines"},
            },
            "required": ["text"],
        },
    })
}

fn retract_highlight_tool() -> Value {
    json!({
        "name": RETRACT_HIGHLIGHT,
        "description": "Take back a highlight of yours that turned out wrong or no longer applies, saying why. It leaves the user's \"for you\" list and stops counting as waiting on them; its card stays in the chat, muted, with \"Retracted: <reason>\". Only while the user hasn't marked it seen: after that it's an error that says so (say what changed in a reply instead). If it changed, retract it and highlight the new state.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "The highlight's id, from `highlight`'s result"},
                "reason": {"type": "string", "description": "Why it no longer applies, a line; the user sees it"},
            },
            "required": ["id", "reason"],
        },
    })
}

fn withdraw_question_tool() -> Value {
    json!({
        "name": WITHDRAW_QUESTION,
        "description": "Take back a question you asked with `ask_user` that no longer applies, because the user answered it in the chat or you decided otherwise, saying why. The question closes for the user and stops counting as waiting; the chat says \"Withdrawn: <reason>\". Nothing is sent to you.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "The question's id, from `ask_user`'s result"},
                "reason": {"type": "string", "description": "Why it no longer applies, a line; the user sees it"},
            },
            "required": ["id", "reason"],
        },
    })
}

fn text(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

async fn call(state: &AppState, session: Uuid, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let row = match session_kind(state, session).await {
        Ok(row) => row,
        Err(e) => return text(e, true),
    };
    if !tools(state, &row).await.iter().any(|t| t["name"] == name) {
        return text(format!("{name} isn't one of this session's tools"), true);
    }
    let result = match name {
        "submit_review" => reviews::submit(state, session, &arguments).await,
        "request_review" => reviews::request(state, session, &arguments).await,
        "ask_user" => ask_user(state, session, &arguments).await,
        HIGHLIGHT => highlight(state, session, &arguments).await,
        RETRACT_HIGHLIGHT => retract_highlight(state, session, &arguments).await,
        WITHDRAW_QUESTION => withdraw_question(state, session, &arguments).await,
        LIST_GRANTS => list_grants_text(state, session).await,
        "get_proposal" => get_proposal(state, session, &arguments).await,
        "withdraw_proposal" => withdraw_proposal(state, session, &arguments).await,
        "spawn_session" => subsessions::spawn(state, session, &arguments).await,
        "list_sessions" => subsessions::list(state, session).await,
        "message_session" => subsessions::message(state, session, &arguments).await,
        "park_session" => subsessions::park(state, session, &arguments).await,
        work::agent::LIST | work::agent::UPDATE => {
            work::agent::call(&state.db, &row.context(), name, &arguments).await
        }
        _ => propose(state, session, name, arguments).await,
    };
    match result {
        Ok(message) => text(message, false),
        Err(e) => text(e, true),
    }
}

async fn highlight(state: &AppState, session: Uuid, arguments: &Value) -> Result<String, String> {
    let text = arguments["text"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("give the `text` to highlight")?;
    if text.chars().count() > HIGHLIGHT_LIMIT {
        return Err(format!(
            "a highlight is read in a list: keep it under {HIGHLIGHT_LIMIT} characters and leave the details in the chat"
        ));
    }
    let row = db::insert_highlight(&state.db, session, text)
        .await
        .map_err(|e| e.to_string())?;
    state.hub.broadcast_highlight(&row.to_api());
    // The id is what ties the agent's tool call to the highlight: its result is in the transcript under the call, where
    // the UI reads it, since the call itself doesn't carry the agent's tool call id.
    Ok(format!(
        "Highlighted (highlight {}). It stays in the user's list until they mark it seen; nothing waits on it.",
        row.id
    ))
}

async fn ask_user(state: &AppState, session: Uuid, arguments: &Value) -> Result<String, String> {
    let id = state
        .hub
        .ask(session, arguments["questions"].clone())
        .await?;
    Ok(format!(
        "Asked (question {id}). The answer arrives later as a message; carry on with what doesn't depend on it. If it no longer applies, `withdraw_question` takes it back."
    ))
}

/// What the agent may give as the reason for taking something back, in characters.
const REASON_LIMIT: usize = 500;

fn reason(arguments: &Value) -> Result<&str, String> {
    let reason = arguments["reason"]
        .as_str()
        .map(str::trim)
        .unwrap_or_default();
    if reason.is_empty() {
        return Err("give the `reason` it no longer applies; the user sees it".into());
    }
    if reason.chars().count() > REASON_LIMIT {
        return Err(format!("keep the reason under {REASON_LIMIT} characters"));
    }
    Ok(reason)
}

async fn withdraw_question(
    state: &AppState,
    session: Uuid,
    arguments: &Value,
) -> Result<String, String> {
    let id = proposal_id(arguments).ok_or("give the question's id, as `ask_user` returned it")?;
    state
        .hub
        .withdraw_question(session, id, reason(arguments)?)
        .await?;
    Ok("Withdrawn: the question is closed for the user, with your reason.".into())
}

async fn retract_highlight(
    state: &AppState,
    session: Uuid,
    arguments: &Value,
) -> Result<String, String> {
    let id = proposal_id(arguments).ok_or("give the highlight's id, as `highlight` returned it")?;
    let reason = reason(arguments)?;
    match db::retract_highlight(&state.db, session, id, reason)
        .await
        .map_err(|e| e.to_string())?
    {
        db::Retraction::Retracted(row) => {
            state.hub.broadcast_highlight(&row.to_api());
            Ok("Retracted: it leaves the user's list, and stays in the chat, muted, with your reason. If it changed, highlight the new state.".into())
        }
        db::Retraction::Seen => Err("the user has already seen it, so it can't be taken back; say what changed in a reply, or highlight the new state".into()),
        db::Retraction::AlreadyRetracted => Err("it was retracted already".into()),
        db::Retraction::NotFound => Err("no such highlight of yours".into()),
    }
}

async fn list_grants_text(state: &AppState, session: Uuid) -> Result<String, String> {
    match db::get_session(&state.db, session).await {
        Ok(Some(row)) => Ok(grant_list(&row.to_api().grants, &row.role())),
        _ => Err("couldn't load the session".into()),
    }
}

fn proposal_id(arguments: &Value) -> Option<Uuid> {
    arguments["id"].as_str().and_then(|s| s.parse().ok())
}

async fn get_proposal(
    state: &AppState,
    session: Uuid,
    arguments: &Value,
) -> Result<String, String> {
    let proposal = match proposal_id(arguments) {
        Some(id) => db::get_proposal(&state.db, id).await.ok().flatten(),
        None => None,
    };
    match proposal {
        Some(p) if p.session_id == session => {
            Ok(serde_json::to_string_pretty(&p.to_api()).unwrap_or_default())
        }
        _ => Err("no such proposal".into()),
    }
}

async fn withdraw_proposal(
    state: &AppState,
    session: Uuid,
    arguments: &Value,
) -> Result<String, String> {
    let id = proposal_id(arguments).ok_or("give the proposal's id")?;
    let reason = arguments["reason"]
        .as_str()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    match db::withdraw_proposal(&state.db, id, session, reason).await {
        Ok(Some(row)) => {
            state.hub.broadcast_proposal(&row.to_api());
            if let Some(review) = row.review_session_id {
                reviews::finish(state, review);
            }
            Ok(format!("Proposal {id} is withdrawn."))
        }
        Ok(None) => Err(
            "no undecided proposal of yours with that id; decided ones can't be withdrawn".into(),
        ),
        Err(e) => Err(format!("couldn't withdraw it: {e}")),
    }
}

async fn propose(
    state: &AppState,
    session: Uuid,
    tool: &str,
    arguments: Value,
) -> Result<String, String> {
    let proposal = proposals::propose(state, session, tool, arguments)
        .await
        .map_err(|e| format!("Proposal refused: {e}"))?;
    // A code review asks no human: it ends with its findings, and nothing is carried out.
    if proposal.kind == proposals::CODE_REVIEW {
        return Ok(proposals::code_review_started(&proposal));
    }
    Ok(match proposal.status {
        ProposalStatus::InReview => format!(
            "Proposal {} created; another model is reviewing it before a human sees it. Its findings, if any, come \
             back to you.",
            proposal.id
        ),
        ProposalStatus::Pending => format!(
            "Proposal {} created and waiting for human review. Nothing has happened yet; once it's approved sloppod \
             carries it out. Check its status with get_proposal.",
            proposal.id
        ),
        _ => format!(
            "Proposal {} was approved by a standing approval and is being carried out; you'll be told how it went. \
             Check its status with get_proposal.",
            proposal.id
        ),
    })
}

fn grant_list(grants: &[sloppod_protocol::api::Grant], role: &SessionRole) -> String {
    let mut lines: Vec<String> = grants
        .iter()
        .map(|g| {
            let until = g
                .expires_at
                .as_ref()
                .map(|at| format!(" (until {at})"))
                .unwrap_or_default();
            format!(
                "- {}: {}{until}",
                g.capability,
                g.description.as_deref().unwrap_or_default()
            )
        })
        .collect();
    let next = role
        .repo_advice()
        .map(|advice| format!("; for another repository, {advice}"))
        .unwrap_or_default();
    lines.push(format!("Anything else is refused{next}."));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_hears_when_its_tools_change_from_what_it_was_given() {
        let streams = ToolStreams::default();
        let session = Uuid::new_v4();
        let before = [json!({"name": "ask_user"})];
        let after = [json!({"name": "ask_user"}), json!({"name": "list_work"})];
        // Not listed yet: it gets the current list when it asks.
        assert!(!streams.changed(session, &after));
        streams.listed(session, &before);
        // No stream open: nobody to tell, and it still has what it was given.
        assert!(!streams.changed(session, &after));
        let mut changed = streams.subscribe(session);
        assert!(!streams.changed(session, &before));
        assert!(streams.changed(session, &after));
        assert!(changed.try_recv().is_ok());
        assert!(!streams.changed(session, &after), "told once");
        assert!(
            streams.changed(session, &before),
            "and again when they change back"
        );
        streams.forget(session);
        assert!(!streams.changed(session, &after));
    }

    #[test]
    fn a_change_missed_while_no_stream_was_open_is_told_when_one_opens() {
        let streams = ToolStreams::default();
        let session = Uuid::new_v4();
        let before = [json!({"name": "ask_user"})];
        let after = [json!({"name": "ask_user"}), json!({"name": "list_work"})];
        streams.listed(session, &before);
        let stream = streams.subscribe(session);
        drop(stream);
        // The grant changes while the stream is closed: nobody hears.
        assert!(!streams.changed(session, &after));
        // Reopened with no further change, it's told of the one it missed, until it lists the tools again.
        let _stream = streams.subscribe(session);
        assert!(streams.stale(session, &after));
        streams.listed(session, &after);
        assert!(!streams.stale(session, &after));
        // Never listed: nothing to be stale.
        assert!(!streams.stale(Uuid::new_v4(), &after));
    }

    #[test]
    fn only_a_session_the_user_talks_to_can_highlight() {
        assert!(highlight_offered(&SessionRole::Work));
        assert!(!highlight_offered(&SessionRole::SubSession {
            parent: "p".into()
        }));
        assert!(!highlight_offered(&SessionRole::Review));
        assert!(!highlight_offered(&SessionRole::Utility));
        assert_eq!(highlight_tool()["name"], HIGHLIGHT);
        assert_eq!(retract_highlight_tool()["name"], RETRACT_HIGHLIGHT);
        assert_eq!(withdraw_question_tool()["name"], WITHDRAW_QUESTION);
    }

    #[test]
    fn the_grant_list_points_to_request_repository_only_where_it_exists() {
        assert!(grant_list(&[], &SessionRole::Work).contains("request_repository"));
        let sub = grant_list(&[], &SessionRole::SubSession { parent: "p".into() });
        assert!(!sub.contains("request_repository"), "{sub}");
        assert!(sub.contains("your report"), "{sub}");
        assert_eq!(
            grant_list(&[], &SessionRole::Review),
            "Anything else is refused."
        );
    }

    /// A session whose tools a test sets, with the runtime's own change tracking.
    struct Fake {
        id: Uuid,
        streams: ToolStreams,
        tools: StdMutex<Vec<Value>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                id: Uuid::new_v4(),
                streams: ToolStreams::default(),
                tools: StdMutex::new(vec![json!({"name": "ask_user"})]),
            }
        }

        /// What a grant change does: the session has another tool, and its agent is told.
        fn grant(&self) -> bool {
            let mut tools = self.tools.lock().unwrap();
            tools.push(json!({"name": "list_work"}));
            self.streams.changed(self.id, &tools)
        }
    }

    impl Session for Fake {
        fn version(&self) -> &str {
            "test"
        }

        async fn tools(&self) -> Result<Vec<Value>, String> {
            Ok(self.tools.lock().unwrap().clone())
        }

        async fn call(&self, params: &Value) -> Value {
            text(
                format!("called {}", params["name"].as_str().unwrap()),
                false,
            )
        }

        fn listed(&self, tools: &[Value]) {
            self.streams.listed(self.id, tools);
        }

        fn subscribe(&self) -> broadcast::Receiver<()> {
            self.streams.subscribe(self.id)
        }

        fn stale(&self, tools: &[Value]) -> bool {
            self.streams.stale(self.id, tools)
        }
    }

    const KEY_INFO: &str = META_SERVER_INFO;

    fn modern_meta(version: &str) -> Value {
        json!({
            META_VERSION: version,
            META_CAPABILITIES: {},
            META_CLIENT_INFO: {"name": "test", "version": "1"},
        })
    }

    fn request(id: impl Into<Value>, method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id.into(), "method": method, "params": params})
    }

    /// Posts as a client does: the stateless era's headers say what the body does, the version being `version` where
    /// it's given.
    async fn post(session: &Fake, message: Value, version: Option<&str>) -> (StatusCode, Value) {
        let text = |v: &Value| v.as_str().map(str::to_owned);
        let meta_version = text(&message["params"]["_meta"][META_VERSION]);
        let (method, name) = (text(&message["method"]), text(&message["params"]["name"]));
        let headers = Headers {
            version: version.or(meta_version.as_deref()),
            method: method.as_deref(),
            name: name.as_deref(),
        };
        match serve(session, message, &headers).await {
            Outcome::Reply(status, body) => (status, body),
            _ => panic!("expected a reply"),
        }
    }

    async fn listening(session: &Fake, message: Value) -> Messages {
        let headers = Headers {
            version: Some(STATELESS),
            method: Some("subscriptions/listen"),
            name: None,
        };
        match serve(session, message, &headers).await {
            Outcome::Listen(messages) => messages,
            _ => panic!("expected a stream"),
        }
    }

    async fn next(messages: &mut Messages) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(1), messages.next())
            .await
            .expect("a message")
            .expect("the stream is open")
    }

    #[tokio::test]
    async fn a_2025_client_initializes_lists_and_calls_and_hears_of_a_change_on_the_get_stream() {
        let session = Fake::new();
        let (status, body) = post(
            &session,
            request(1, "initialize", json!({"protocolVersion": "2025-11-25"})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let result = &body["result"];
        assert_eq!(result["protocolVersion"], "2025-11-25");
        assert_eq!(result["serverInfo"]["name"], "sloppod");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
        assert!(result.get("resultType").is_none(), "{result}");
        // The 2025 client also sends no `_meta` afterwards.
        let (_, body) = post(&session, request(2, "ping", json!({})), Some("2025-11-25")).await;
        assert_eq!(body["result"], json!({}));
        let (_, body) = post(&session, request(3, "tools/list", json!({})), None).await;
        assert_eq!(body["result"], json!({"tools": [{"name": "ask_user"}]}));
        let (_, body) = post(
            &session,
            request(4, "tools/call", json!({"name": "ask_user"})),
            None,
        )
        .await;
        assert_eq!(body["result"]["content"][0]["text"], "called ask_user");
        assert!(body["result"].get("resultType").is_none(), "{body}");

        let mut stream = tool_notices(&session).await;
        assert!(session.grant());
        assert_eq!(
            next(&mut stream).await,
            json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        );
    }

    #[tokio::test]
    async fn a_2026_client_discovers_lists_and_calls_and_hears_of_a_change_on_subscriptions_listen()
    {
        let session = Fake::new();
        let meta = modern_meta(STATELESS);
        let (status, body) = post(
            &session,
            request(1, "server/discover", json!({"_meta": meta})),
            Some(STATELESS),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let result = &body["result"];
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["supportedVersions"], json!(SUPPORTED));
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
        assert_eq!(result["_meta"][KEY_INFO]["name"], "sloppod");
        assert!(result["instructions"].is_string());

        let (_, body) = post(
            &session,
            request(2, "tools/list", json!({"_meta": meta})),
            None,
        )
        .await;
        assert_eq!(body["result"]["resultType"], "complete");
        assert_eq!(body["result"]["tools"], json!([{"name": "ask_user"}]));
        assert_eq!(body["result"]["_meta"][KEY_INFO]["version"], "test");
        let (_, body) = post(
            &session,
            request(3, "tools/call", json!({"_meta": meta, "name": "ask_user"})),
            None,
        )
        .await;
        assert_eq!(body["result"]["resultType"], "complete");
        assert_eq!(body["result"]["content"][0]["text"], "called ask_user");

        let mut stream = listening(
            &session,
            request(
                7,
                "subscriptions/listen",
                json!({"_meta": meta, "notifications": {"toolsListChanged": true}}),
            ),
        )
        .await;
        let acknowledged = next(&mut stream).await;
        assert_eq!(
            acknowledged["method"],
            "notifications/subscriptions/acknowledged"
        );
        assert_eq!(
            acknowledged["params"]["notifications"],
            json!({"toolsListChanged": true})
        );
        assert_eq!(acknowledged["params"]["_meta"][META_SUBSCRIPTION], 7);
        assert!(session.grant());
        let notice = next(&mut stream).await;
        assert_eq!(notice["method"], "notifications/tools/list_changed");
        assert_eq!(notice["params"]["_meta"][META_SUBSCRIPTION], 7);
    }

    #[tokio::test]
    async fn a_listener_is_told_of_a_change_it_missed_after_the_acknowledgment() {
        let session = Fake::new();
        let meta = modern_meta(STATELESS);
        post(
            &session,
            request(1, "tools/list", json!({"_meta": meta})),
            None,
        )
        .await;
        // No stream is open, so nobody hears of it.
        assert!(!session.grant());
        let listen = request(
            "l",
            "subscriptions/listen",
            json!({"_meta": meta, "notifications": {"toolsListChanged": true}}),
        );
        let mut stream = listening(&session, listen).await;
        let acknowledged = next(&mut stream).await;
        assert_eq!(
            acknowledged["method"],
            "notifications/subscriptions/acknowledged"
        );
        assert_eq!(acknowledged["params"]["_meta"][META_SUBSCRIPTION], "l");
        let notice = next(&mut stream).await;
        assert_eq!(notice["method"], "notifications/tools/list_changed");
        assert_eq!(notice["params"]["_meta"][META_SUBSCRIPTION], "l");
    }

    #[tokio::test]
    async fn a_listener_that_asked_for_no_notifications_is_acknowledged_and_told_nothing() {
        let session = Fake::new();
        let listen = request(
            1,
            "subscriptions/listen",
            json!({"_meta": modern_meta(STATELESS)}),
        );
        let mut stream = listening(&session, listen).await;
        assert_eq!(
            next(&mut stream).await["params"]["notifications"],
            json!({})
        );
        session.streams.listed(session.id, &[]);
        session.grant();
        let more = tokio::time::timeout(std::time::Duration::from_millis(100), stream.next()).await;
        assert!(more.is_err(), "{more:?}");
    }

    #[tokio::test]
    async fn an_unsupported_protocol_version_is_refused_in_the_stateless_era_and_negotiated_in_the_other()
     {
        let session = Fake::new();
        let expected = json!({
            "message": "Unsupported protocol version",
            "data": {"supported": SUPPORTED, "requested": "1900-01-01"},
        });
        let (status, body) = post(
            &session,
            request(1, "tools/list", json!({"_meta": modern_meta("1900-01-01")})),
            Some("1900-01-01"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], -32022);
        assert_eq!(body["error"]["message"], expected["message"]);
        assert_eq!(body["error"]["data"], expected["data"]);
        assert_eq!(body["id"], 1);

        // A handshake client asking for a version it doesn't serve is told the newest the era has, and decides itself.
        for requested in ["1900-01-01", STATELESS] {
            let (status, body) = post(
                &session,
                request(2, "initialize", json!({"protocolVersion": requested})),
                None,
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["result"]["protocolVersion"], "2025-11-25");
        }
        // The oldest a client still offers is served as itself, not negotiated away from it.
        for asked in ["2025-03-26", "2024-11-05"] {
            let (_, body) = post(
                &session,
                request(3, "initialize", json!({"protocolVersion": asked})),
                None,
            )
            .await;
            assert_eq!(body["result"]["protocolVersion"], asked);
        }
        // A 2025 client's version isn't a version of the stateless era either.
        let (status, body) = post(
            &session,
            request(4, "tools/list", json!({"_meta": modern_meta("2025-11-25")})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], -32022);
    }

    #[tokio::test]
    async fn a_2026_request_missing_its_meta_is_invalid_and_a_2026_ping_is_unknown() {
        let session = Fake::new();
        let (status, body) = post(
            &session,
            request(1, "tools/list", json!({})),
            Some(STATELESS),
        )
        .await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (StatusCode::BAD_REQUEST, &json!(-32602))
        );
        let (status, body) = post(
            &session,
            request(
                2,
                "tools/list",
                json!({"_meta": {META_CLIENT_INFO: {"name": "x", "version": "1"}}}),
            ),
            None,
        )
        .await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (StatusCode::BAD_REQUEST, &json!(-32602))
        );
        let (status, body) = post(
            &session,
            request(3, "tools/list", json!({"_meta": {META_VERSION: STATELESS}})),
            None,
        )
        .await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (StatusCode::BAD_REQUEST, &json!(-32602))
        );

        let (status, body) = post(
            &session,
            request(4, "ping", json!({"_meta": modern_meta(STATELESS)})),
            None,
        )
        .await;
        assert_eq!(
            (status, &body["error"]["code"]),
            (StatusCode::NOT_FOUND, &json!(-32601))
        );
    }

    #[tokio::test]
    async fn a_2026_request_whose_headers_dont_match_its_body_is_refused_before_it_runs() {
        let session = Fake::new();
        let call = || {
            request(
                1,
                "tools/call",
                json!({"_meta": modern_meta(STATELESS), "name": "ask_user"}),
            )
        };
        let good = Headers {
            version: Some(STATELESS),
            method: Some("tools/call"),
            name: Some("ask_user"),
        };
        let Outcome::Reply(status, body) = serve(&session, call(), &good).await else {
            panic!("expected a reply");
        };
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"]["content"][0]["text"], "called ask_user");
        // The name may come Base64-encoded.
        let encoded = Headers {
            name: Some("=?base64?YXNrX3VzZXI=?="),
            ..good
        };
        assert!(matches!(
            serve(&session, call(), &encoded).await,
            Outcome::Reply(StatusCode::OK, _)
        ));

        let cases = [
            (
                Headers {
                    version: None,
                    ..good
                },
                "MCP-Protocol-Version header is missing",
            ),
            (
                Headers {
                    version: Some("2025-11-25"),
                    ..good
                },
                "MCP-Protocol-Version header value '2025-11-25'",
            ),
            (
                Headers {
                    method: None,
                    ..good
                },
                "Mcp-Method header is missing",
            ),
            (
                Headers {
                    method: Some("tools/list"),
                    ..good
                },
                "Mcp-Method header value 'tools/list'",
            ),
            (Headers { name: None, ..good }, "Mcp-Name header is missing"),
            (
                Headers {
                    name: Some("list_work"),
                    ..good
                },
                "Mcp-Name header value 'list_work'",
            ),
            (
                Headers {
                    name: Some("=?base64?bGlzdF93b3Jr?="),
                    ..good
                },
                "Mcp-Name header value 'list_work'",
            ),
            (
                Headers {
                    name: Some("=?base64?!!?="),
                    ..good
                },
                "not valid Base64",
            ),
        ];
        for (headers, why) in cases {
            let Outcome::Reply(status, body) = serve(&session, call(), &headers).await else {
                panic!("expected a reply");
            };
            assert_eq!(status, StatusCode::BAD_REQUEST, "{why}");
            assert_eq!(body["error"]["code"], -32020, "{why}");
            let message = body["error"]["message"].as_str().unwrap();
            assert!(message.contains(why), "{message} should say {why}");
        }
    }

    #[tokio::test]
    async fn a_notification_gets_no_reply() {
        let session = Fake::new();
        let notification = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(matches!(
            serve(&session, notification, &Headers::default()).await,
            Outcome::Accepted
        ));
    }
}
