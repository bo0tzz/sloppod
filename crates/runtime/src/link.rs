//! The supervisor link (ADR 17): session pods connect outward, authenticate with their projected
//! ServiceAccount token, receive their launch spec and bridge the agent's ACP stdio.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use serde_json::{Value, json};
use sloppod_contracts::capability;
use sloppod_contracts::{AgentContext, MemoryContext};
use sloppod_protocol::api::SessionKind;
use sloppod_protocol::link::{
    AgentReady, FromSupervisor, Launch, Repo, RunningAgent, ToSupervisor, TranscriptFile,
    TranscriptRef,
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::adapter::{self, PLACEHOLDER};
use crate::github::GitIdentity;
use crate::placement::WORKDIR;
use crate::pods::SESSION_LABEL;
use crate::session::Hub;
use crate::{AppState, db};

pub const REPO_DIR: &str = "/workspace/repo";
/// Where sessions without a repository work.
pub const WORK_DIR: &str = "/workspace/work";

/// Where other repositories the session was given are cloned.
pub const REPOS_DIR: &str = "/workspace/repos";

fn work_dir(session: &db::SessionRow) -> &'static str {
    if session.repo.is_some() {
        REPO_DIR
    } else {
        WORK_DIR
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    fn from_value(value: &Value) -> Self {
        Self {
            code: value["code"].as_i64().unwrap_or(-32603),
            message: value["message"].as_str().unwrap_or("error").to_owned(),
            data: value.get("data").cloned(),
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// JSON-RPC to the agent, carried over the supervisor link.
pub struct AgentConn {
    tx: mpsc::UnboundedSender<ToSupervisor>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>,
    next_id: AtomicI64,
    /// Commands run in the pod (`ToSupervisor::Run`) waiting for their result.
    runs: Mutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>,
    next_run: std::sync::atomic::AtomicU64,
    closed: std::sync::atomic::AtomicBool,
}

impl AgentConn {
    pub(crate) fn new(tx: mpsc::UnboundedSender<ToSupervisor>) -> Self {
        Self {
            tx,
            pending: Mutex::default(),
            next_id: AtomicI64::new(1),
            runs: Mutex::default(),
            next_run: std::sync::atomic::AtomicU64::new(1),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn send_acp(&self, message: Value) {
        let _ = self.tx.send(ToSupervisor::Acp { message });
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.send_request(method, params).1.await
    }

    /// Sends a request and returns its id, which a supervisor reports while it's open, with its answer to come.
    pub fn send_request(
        &self,
        method: &str,
        params: Value,
    ) -> (i64, impl Future<Output = Result<Value, RpcError>> + use<>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        self.send_acp(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let answer = async move {
            rx.await
                .unwrap_or_else(|_| Err(RpcError::new(-32002, "the session's agent disconnected")))
        };
        (id, answer)
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send_acp(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub fn respond(&self, id: Value, result: Result<Value, RpcError>) {
        let message = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        self.send_acp(message);
    }

    pub fn sync_transcript(&self) {
        let _ = self.tx.send(ToSupervisor::SyncTranscript);
    }

    fn resync_transcript(&self, name: String) {
        let _ = self.tx.send(ToSupervisor::ResyncTranscript { name });
    }

    /// Takes over a request an earlier link sent the agent, so its answer still reaches someone.
    pub(crate) fn adopt(&self, id: i64) -> oneshot::Receiver<Result<Value, RpcError>> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        self.next_id.fetch_max(id + 1, Ordering::Relaxed);
        rx
    }

    /// Tells the supervisor the agent's session is up, so it can report the agent if the link drops.
    pub fn ready(&self, ready: AgentReady) {
        let _ = self.tx.send(ToSupervisor::AgentReady(ready));
    }

    pub(crate) fn handle_response(&self, message: &Value) {
        let Some(id) = message["id"].as_i64() else {
            return;
        };
        let Some(tx) = self.pending.lock().expect("pending lock").remove(&id) else {
            return;
        };
        let result = match message.get("error") {
            Some(error) => Err(RpcError::from_value(error)),
            None => Ok(message["result"].clone()),
        };
        let _ = tx.send(result);
    }

    /// Runs a command in the pod, apart from the agent; its output, or why it failed.
    pub async fn run(
        &self,
        command: Vec<String>,
        env: Vec<(String, String)>,
        input: String,
        timeout: Duration,
    ) -> Result<String, String> {
        const GONE: &str = "the session's pod disconnected";
        let id = self.next_run.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            // Under the lock `close` clears runs with, so a run is either refused here or failed there.
            let mut runs = self.runs.lock().expect("runs lock");
            if self.is_closed() {
                return Err(GONE.to_owned());
            }
            runs.insert(id, tx);
        }
        let sent = self.tx.send(ToSupervisor::Run {
            id,
            command,
            env,
            input,
            timeout_secs: timeout.as_secs(),
        });
        if sent.is_err() {
            self.runs.lock().expect("runs lock").remove(&id);
            return Err(GONE.to_owned());
        }
        rx.await.unwrap_or_else(|_| Err(GONE.to_owned()))
    }

    pub(crate) fn run_result(&self, id: u64, output: Option<String>, error: Option<String>) {
        if let Some(tx) = self.runs.lock().expect("runs lock").remove(&id) {
            let _ = tx.send(output.ok_or_else(|| error.unwrap_or_default()));
        }
    }

    /// Whether the link this connection was on has gone.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Fails every outstanding request and run; the link is gone.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.pending.lock().expect("pending lock").clear();
        let mut runs = self.runs.lock().expect("runs lock");
        runs.clear();
    }
}

/// The session of the pod whose token a request carries, or the response refusing it.
async fn calling_session(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Uuid, (StatusCode, String)> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = token else {
        return Err((StatusCode::UNAUTHORIZED, "missing bearer token".to_owned()));
    };
    authenticate(state, token).await.map_err(|e| {
        tracing::warn!(error = %e, "supervisor authentication failed");
        (StatusCode::FORBIDDEN, e.to_string())
    })
}

pub async fn handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let session = match calling_session(&state, &headers).await {
        Ok(session) => session,
        Err(refused) => return refused.into_response(),
    };
    // A transcript sync is one message, whole and base64-encoded; axum's default 16 MiB frame limit silently broke
    // the link once a session's transcript passed about 12 MB.
    ws.max_message_size(256 << 20)
        .max_frame_size(256 << 20)
        .on_upgrade(move |socket| run(state, session, socket))
}

/// The session whose pod a ServiceAccount token was issued to, checked by the Kubernetes API.
pub(crate) async fn authenticate(state: &AppState, token: &str) -> anyhow::Result<Uuid> {
    let review = TokenReview {
        spec: TokenReviewSpec {
            token: Some(token.to_owned()),
            audiences: Some(vec!["sloppod".into()]),
        },
        ..Default::default()
    };
    let review = Api::<TokenReview>::all(state.kube.clone())
        .create(&PostParams::default(), &review)
        .await?;
    let status = review.status.context("token review without status")?;
    if status.authenticated != Some(true) {
        bail!(
            "token not authenticated: {}",
            status.error.unwrap_or_default()
        );
    }
    let user = status.user.context("token review without user")?;
    let expected = format!(
        "system:serviceaccount:{}:{}",
        state.config.namespace, state.config.session_service_account
    );
    if user.username.as_deref() != Some(expected.as_str()) {
        bail!("unexpected identity {:?}", user.username);
    }
    let pod = user
        .extra
        .as_ref()
        .and_then(|e| e.get("authentication.kubernetes.io/pod-name"))
        .and_then(|v| v.first())
        .context("token is not bound to a pod")?;
    if let Some(session) = state.pods.session_for_pod(pod) {
        return Ok(session);
    }
    let pod = Api::<Pod>::namespaced(state.kube.clone(), &state.config.namespace)
        .get(pod)
        .await?;
    pod.metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(SESSION_LABEL))
        .and_then(|s| s.parse().ok())
        .context("pod is not a session pod")
}

/// How much of a transcript file the runtime reads at once while sending it.
const TRANSCRIPT_CHUNK: i64 = 1 << 20;

/// Sends a pod one of its session's transcript files, a chunk at a time, so a long session's transcript is never
/// all in memory here.
pub async fn transcript(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let session = match calling_session(&state, &headers).await {
        Ok(session) => session,
        Err(refused) => return refused.into_response(),
    };
    if !sloppod_protocol::link::is_transcript_file_name(&name) {
        return (StatusCode::BAD_REQUEST, "not a transcript file name").into_response();
    }
    let len = match db::transcript_len(&state.db, session, &name).await {
        Ok(Some(len)) => len,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let chunks = futures::stream::try_unfold(0, move |offset| {
        let (state, name) = (state.clone(), name.clone());
        async move {
            if offset >= len {
                return Ok(None);
            }
            let chunk =
                db::transcript_chunk(&state.db, session, &name, offset, TRANSCRIPT_CHUNK).await?;
            if chunk.is_empty() {
                return Ok(None);
            }
            let next = offset + chunk.len() as i64;
            Ok::<_, sqlx::Error>(Some((bytes::Bytes::from(chunk), next)))
        }
    });
    (
        [(axum::http::header::CONTENT_LENGTH, len.to_string())],
        Body::from_stream(chunks),
    )
        .into_response()
}

/// Commits are by whoever pushes them: the owner's GitHub App.
async fn git_identity(state: &AppState, owner: &str) -> GitIdentity {
    let identity = match state.credentials.github(owner).await {
        Ok(app) => app.identity().await,
        Err(e) => Err(e),
    };
    identity.unwrap_or_else(|e| {
        tracing::warn!(%owner, error = %e, "no git identity from a GitHub App");
        GitIdentity {
            name: "sloppod".to_owned(),
            email: "sloppod@localhost".to_owned(),
        }
    })
}

async fn launch_for(
    state: &AppState,
    session: &db::SessionRow,
    context: &AgentContext,
    fetch_transcript: bool,
) -> anyhow::Result<(Launch, sloppod_contracts::AgentLaunch)> {
    let agent = state
        .agents
        .get(&session.agent)
        .with_context(|| format!("no agent called {}", session.agent))?
        .launch(context);
    let (transcript, transcript_files) = if fetch_transcript {
        let files = db::transcript_lens(&state.db, session.id)
            .await?
            .into_iter()
            .map(|(name, len)| TranscriptRef {
                name,
                len: len as u64,
            })
            .collect();
        (Vec::new(), files)
    } else {
        let files = db::transcripts(&state.db, session.id)
            .await?
            .into_iter()
            .map(|(name, content)| TranscriptFile {
                name,
                content_b64: base64::engine::general_purpose::STANDARD.encode(content),
            })
            .collect();
        (files, Vec::new())
    };
    let identity = git_identity(state, &session.owner).await;
    let mut env = agent.env.clone();
    env.extend([
        ("GH_TOKEN".to_owned(), PLACEHOLDER.to_owned()),
        ("GITHUB_TOKEN".to_owned(), PLACEHOLDER.to_owned()),
        ("GIT_AUTHOR_NAME".to_owned(), identity.name.clone()),
        ("GIT_AUTHOR_EMAIL".to_owned(), identity.email.clone()),
        ("GIT_COMMITTER_NAME".to_owned(), identity.name),
        ("GIT_COMMITTER_EMAIL".to_owned(), identity.email),
    ]);
    // The owner's clock for the agent and its commands, and so for the date Claude Code puts in its prompt (#253).
    match db::user_zone(&state.db, &session.owner).await {
        Ok(zone) => env.push(("TZ".to_owned(), zone.name().to_owned())),
        Err(e) => tracing::warn!(session = %session.id, error = %e, "reading the owner's timezone"),
    }
    let launch = Launch {
        session_id: session.id,
        files: agent
            .files
            .iter()
            .map(|f| sloppod_protocol::link::File {
                path: f.path.clone(),
                content: f.content.clone(),
                overwrite: f.overwrite,
            })
            .collect(),
        transcript_dir: agent.transcript_dir.clone(),
        transcript,
        transcript_files,
        ca_pem: Some(state.ca.cert_pem.clone()),
        repo: session
            .repo
            .as_deref()
            .and_then(|repo| {
                context
                    .session
                    .writable_repo_for(repo)
                    .or_else(|| context.session.readable_repo_for(repo))
            })
            .map(|grant| Repo {
                clone_url: format!("https://github.com/{}.git", grant.repo),
                branch: grant.branch.to_owned(),
                path: REPO_DIR.to_owned(),
                commit: context
                    .session
                    .grants
                    .iter()
                    .find(|g| {
                        g.capability == capability::REPO
                            && g.param("repo")
                                .is_some_and(|r| sloppod_contracts::same_repo(r, grant.repo))
                    })
                    .and_then(|g| g.param("commit"))
                    .map(str::to_owned),
                from: context
                    .session
                    .grants
                    .iter()
                    .find(|g| {
                        g.capability == capability::REPO
                            && g.param("repo")
                                .is_some_and(|r| sloppod_contracts::same_repo(r, grant.repo))
                    })
                    .and_then(|g| g.param("from"))
                    .map(str::to_owned),
            }),
        install_tools: session.repo.is_some(),
        command: agent.command.clone(),
        env,
        cwd: work_dir(session).to_owned(),
        runtime_build: state.config.build.clone(),
    };
    Ok((launch, agent))
}

/// The agent's view of its session. With `local_proxy`, its supervisor serves the runtime's endpoints on localhost
/// and attaches the pod's identity; an older supervisor's agent calls the runtime directly.
pub async fn agent_context(
    state: &AppState,
    session: &db::SessionRow,
    local_proxy: bool,
) -> AgentContext {
    let endpoints = if local_proxy {
        format!("http://{}", sloppod_protocol::link::LOCAL_PROXY)
    } else {
        state.config.runtime_url()
    };
    let context = session.context();
    let namespace = crate::capabilities::grant_with(&context, &crate::capabilities::Part::Memory)
        .and_then(|g| g.param("namespace"))
        .map(str::to_owned);
    let memory = match namespace {
        Some(namespace) => match state.credentials.memini(&session.owner).await {
            Ok(Some(_)) => Some(MemoryContext {
                base_url: format!("{endpoints}{}", crate::memory::PREFIX),
                namespace,
            }),
            _ => None,
        },
        None => None,
    };
    AgentContext {
        session: context,
        workdir: WORKDIR.to_owned(),
        repo: session.repo.clone(),
        repo_dir: work_dir(session).to_owned(),
        repos_dir: REPOS_DIR.to_owned(),
        tools_url: format!("{endpoints}/mcp"),
        endpoints: endpoints.clone(),
        // A review session's instructions are the reviewer's own, not the ones the owner's sessions work by.
        instructions: match session.kind {
            SessionKind::Review => Some(crate::reviews::INSTRUCTIONS.to_owned()),
            SessionKind::Work => state.instructions.clone(),
            SessionKind::Utility => None,
        },
        model: session.model.clone(),
        memory,
    }
}

async fn run(state: AppState, session_id: Uuid, socket: WebSocket) {
    let Ok(Some(session)) = db::get_session(&state.db, session_id).await else {
        tracing::warn!(session = %session_id, "link from unknown session");
        return;
    };
    // A parked session's pod may still reach out while it shuts down.
    if matches!(
        session.phase(),
        sloppod_protocol::api::Phase::Stopped | sloppod_protocol::api::Phase::Parked
    ) {
        return;
    }
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ToSupervisor>();
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(20));
        loop {
            tokio::select! {
                message = rx.recv() => {
                    let Some(message) = message else { break };
                    let Ok(text) = serde_json::to_string(&message) else { continue };
                    if sink.send(Message::Text(text.into())).await.is_err() { break; }
                }
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
                }
            }
        }
    });
    let Some((version, running, local_proxy, fetch_transcript)) = hello(&mut stream).await else {
        tracing::warn!(session = %session_id, "supervisor connected without a hello");
        writer.abort();
        return;
    };
    tracing::info!(session = %session_id, %version, attach = running.is_some(), "supervisor connected");
    let agent = Arc::new(AgentConn::new(tx.clone()));
    state.hub.attach_agent(session_id, agent.clone()).await;
    let restart_notice = state.hub.take_restart_notice(session_id).await;

    let agent_launch = match running {
        Some(running) => {
            let _ = tx.send(ToSupervisor::Attach {
                runtime_build: state.config.build.clone(),
            });
            state.hub.adopt_agent(session_id, &agent, running).await;
            None
        }
        None => {
            state
                .hub
                .set_phase(session_id, sloppod_protocol::api::Phase::Starting)
                .await;
            if let Some(notice) = restart_notice {
                state.hub.notify(session_id, notice).await;
            }
            let context = agent_context(&state, &session, local_proxy).await;
            match launch_for(&state, &session, &context, fetch_transcript).await {
                Ok((launch, agent_launch)) => {
                    let _ = tx.send(ToSupervisor::Launch(Box::new(launch)));
                    Some(agent_launch)
                }
                Err(e) => {
                    tracing::error!(session = %session_id, error = %e, "building launch spec");
                    agent_failed(
                        &state,
                        session_id,
                        "The agent couldn't be launched",
                        &format!("{e:#}"),
                    )
                    .await;
                    state.hub.detach_agent(session_id, &agent).await;
                    writer.abort();
                    return;
                }
            }
        }
    };

    // Transcript replacements being put together on this link: file name -> its whole length.
    let mut staging = HashMap::new();
    // An agent that couldn't start its session ends the link, so the supervisor starts it again after a pause.
    let (start_failed, mut start_failures) = mpsc::unbounded_channel::<()>();
    loop {
        let next = tokio::select! {
            next = tokio::time::timeout(LINK_TIMEOUT, stream.next()) => next,
            _ = start_failures.recv() => break,
        };
        let message = match next {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(e))) => {
                tracing::warn!(session = %session_id, error = %e, "the supervisor link failed");
                break;
            }
            Ok(None) => break,
            Err(_) => {
                tracing::warn!(session = %session_id, "no word from the supervisor; dropping the link");
                break;
            }
        };
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            _ => continue,
        };
        let message: FromSupervisor = match serde_json::from_str(&text) {
            Ok(message) => message,
            Err(e) => {
                tracing::warn!(session = %session_id, error = %e, "bad supervisor message");
                continue;
            }
        };
        match message {
            FromSupervisor::Hello { .. } => {}
            FromSupervisor::Status { message } => {
                tracing::info!(session = %session_id, %message, "supervisor status");
                match db::set_agent_status(&state.db, session_id, &message).await {
                    Ok(()) => state.hub.broadcast_session(session_id).await,
                    Err(e) => {
                        tracing::warn!(session = %session_id, error = %e, "storing the agent's status")
                    }
                }
            }
            FromSupervisor::AgentStarted => {
                let Some(agent_launch) = agent_launch.clone() else {
                    continue;
                };
                let (state, agent, session) = (state.clone(), agent.clone(), session.clone());
                let start_failed = start_failed.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        start_agent_session(&state, &agent, &session, &agent_launch).await
                    {
                        tracing::error!(session = %session.id, error = %e, "starting agent session");
                        // A link that went down took the startup's requests with it; that isn't the agent failing.
                        if !agent.is_closed() {
                            agent_failed(
                                &state,
                                session.id,
                                "The agent couldn't start",
                                &format!("{e:#}"),
                            )
                            .await;
                            let _ = start_failed.send(());
                        }
                    }
                });
            }
            FromSupervisor::Acp { message } => {
                from_agent(&state.hub, session_id, &agent, message).await
            }
            FromSupervisor::Transcript { files } => {
                for file in files
                    .into_iter()
                    .filter(|f| transcript_name_ok(session_id, &f.name))
                {
                    let Ok(content) =
                        base64::engine::general_purpose::STANDARD.decode(&file.content_b64)
                    else {
                        continue;
                    };
                    if let Err(e) =
                        db::upsert_transcript(&state.db, session_id, &file.name, &content).await
                    {
                        tracing::error!(session = %session_id, error = %e, "storing transcript");
                    }
                }
            }
            FromSupervisor::TranscriptAppend {
                name,
                offset,
                content_b64,
                replaces,
            } if transcript_name_ok(session_id, &name) => {
                let Ok(content) = base64::engine::general_purpose::STANDARD.decode(&content_b64)
                else {
                    tracing::warn!(session = %session_id, file = %name, "undecodable transcript sync");
                    continue;
                };
                let stored = store_transcript_piece(
                    &state.db,
                    session_id,
                    &mut staging,
                    &name,
                    offset,
                    &content,
                    replaces,
                )
                .await;
                match stored {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(session = %session_id, file = %name, offset, "transcript out of step; asking for all of it");
                        agent.resync_transcript(name);
                    }
                    Err(e) => {
                        tracing::error!(session = %session_id, error = %e, "storing transcript")
                    }
                }
            }
            // Its name was refused above.
            FromSupervisor::TranscriptAppend { .. } => {}
            FromSupervisor::AgentExited { code } => {
                tracing::warn!(session = %session_id, ?code, "agent exited");
                if code != Some(0) {
                    let how =
                        code.map_or("was killed".to_owned(), |c| format!("exited with code {c}"));
                    agent_failed(
                        &state,
                        session_id,
                        "The agent stopped",
                        &format!("The agent {how}; it's started again."),
                    )
                    .await;
                }
            }
            FromSupervisor::Error { message } => {
                tracing::error!(session = %session_id, %message, "supervisor error");
                agent_failed(
                    &state,
                    session_id,
                    "The session's pod couldn't start the agent",
                    &message,
                )
                .await;
            }
            FromSupervisor::Commands { running } => {
                state.hub.set_commands(session_id, &agent, running).await;
            }
            FromSupervisor::RunResult { id, output, error } => {
                agent.run_result(id, output, error);
            }
            FromSupervisor::ShutdownReport { running } => {
                tracing::info!(session = %session_id, commands = running.len(), "pod stopping with commands running");
                // This agent is about to be stopped with its pod, so the notice waits for the next one.
                state.hub.retire_agent(session_id, &agent).await;
                state
                    .hub
                    .notify(session_id, shutdown_notice(&running))
                    .await;
            }
        }
    }
    tracing::info!(session = %session_id, "supervisor disconnected");
    agent.close();
    state.hub.detach_agent(session_id, &agent).await;
    writer.abort();
}

/// Stores a piece of a transcript sync; returns whether it was in step with the stored copy. A replacement that comes
/// in several pieces is staged, so the stored copy stays whole until the last piece is in.
pub(crate) async fn store_transcript_piece(
    db: &sqlx::PgPool,
    session: Uuid,
    staging: &mut HashMap<String, u64>,
    name: &str,
    offset: u64,
    content: &[u8],
    replaces: Option<u64>,
) -> sqlx::Result<bool> {
    let len = content.len() as u64;
    if offset == 0 {
        match replaces {
            Some(total) if total > len => {
                db::upsert_transcript(db, session, &db::staged_transcript(name), content).await?;
                staging.insert(name.to_owned(), total);
            }
            _ => {
                staging.remove(name);
                db::upsert_transcript(db, session, name, content).await?;
            }
        }
        return Ok(true);
    }
    let Some(&total) = staging.get(name) else {
        return db::append_transcript(db, session, name, offset, content).await;
    };
    if !db::append_transcript(db, session, &db::staged_transcript(name), offset, content).await? {
        staging.remove(name);
        return Ok(false);
    }
    if offset + len >= total {
        staging.remove(name);
        db::promote_transcript(db, session, name).await?;
    }
    Ok(true)
}

/// Shows why the agent failed: on the session while it's started again, and in the transcript the first time.
async fn agent_failed(state: &AppState, session: Uuid, title: &str, error: &str) {
    match db::set_agent_error(&state.db, session, error).await {
        Ok(true) => {
            state
                .hub
                .record(
                    session,
                    json!({"sessionUpdate": "notice", "severity": "error", "title": title, "description": error}),
                )
                .await;
            state.hub.broadcast_session(session).await;
        }
        Ok(false) => state.hub.broadcast_session(session).await,
        Err(e) => tracing::warn!(%session, error = %e, "storing the agent's error"),
    }
}

/// Tells the agent which of its commands its pod's ending cut off.
fn shutdown_notice(running: &[sloppod_protocol::link::RunningProcess]) -> String {
    let mut notice = "sloppod: your session's pod was stopped (to be replaced, parked, or moved) while these of your \
                      commands were still running. They were ended; start again whichever you still need:"
        .to_owned();
    for process in running {
        notice.push_str(&format!("\n- `{}`", process.command));
        if let Some(tail) = &process.output_tail {
            notice.push_str(&format!(", which last wrote:\n```\n{tail}\n```"));
        }
    }
    notice
}

/// A transcript file name the session's pod can be given back; one that isn't is dropped and logged.
fn transcript_name_ok(session: Uuid, name: &str) -> bool {
    let ok = sloppod_protocol::link::is_transcript_file_name(name);
    if !ok {
        tracing::warn!(%session, file = %name, "not storing a transcript file with that name");
    }
    ok
}

async fn start_agent_session(
    state: &AppState,
    agent: &Arc<AgentConn>,
    session: &db::SessionRow,
    launch: &sloppod_contracts::AgentLaunch,
) -> anyhow::Result<()> {
    let init = agent
        .request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": launch.client_capabilities,
                "clientInfo": {"name": "sloppod", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("initialize: {e}"))?;
    state.hub.set_agent_capabilities(init.clone());

    // The session row may have changed since the link opened.
    let session = db::get_session(&state.db, session.id)
        .await?
        .context("session vanished")?;
    let (agent_session_id, response) = match &session.agent_session_id {
        Some(existing) => {
            let mut params = launch.load_session.clone();
            params["sessionId"] = json!(existing);
            params["cwd"] = json!(work_dir(&session));
            state.hub.set_loading(session.id, true).await;
            let response = agent.request("session/load", params).await;
            state.hub.set_loading(session.id, false).await;
            match response {
                Ok(mut response) => {
                    set_mode(agent, &session, launch, existing, &mut response).await;
                    (existing.clone(), response)
                }
                Err(e) => {
                    tracing::warn!(session = %session.id, error = %e, "session/load failed; starting a new agent session");
                    new_agent_session(agent, &session, launch).await?
                }
            }
        }
        None => new_agent_session(agent, &session, launch).await?,
    };
    let agent_state = adapter::agent_state(&response);
    db::set_agent_session(&state.db, session.id, &agent_session_id, &agent_state).await?;
    db::clear_agent_status(&state.db, session.id).await?;
    // Cleared once a turn starts with it, so an agent that goes before then leaves it for the next.
    let pending = db::pending_prompt(&state.db, session.id).await?;
    agent.ready(AgentReady {
        agent_session_id: agent_session_id.clone(),
        capabilities: init,
    });
    state
        .hub
        .agent_ready(session.id, agent, agent_session_id, agent_state, pending)
        .await;
    Ok(())
}

async fn new_agent_session(
    agent: &Arc<AgentConn>,
    session: &db::SessionRow,
    launch: &sloppod_contracts::AgentLaunch,
) -> anyhow::Result<(String, Value)> {
    let mut params = launch.new_session.clone();
    params["cwd"] = json!(work_dir(session));
    let mut response = agent
        .request("session/new", params)
        .await
        .map_err(|e| anyhow::anyhow!("session/new: {e}"))?;
    let agent_session_id = response["sessionId"]
        .as_str()
        .context("session/new without sessionId")?
        .to_owned();
    set_mode(agent, session, launch, &agent_session_id, &mut response).await;
    Ok((agent_session_id, response))
}

async fn set_mode(
    agent: &Arc<AgentConn>,
    session: &db::SessionRow,
    launch: &sloppod_contracts::AgentLaunch,
    agent_session_id: &str,
    response: &mut Value,
) {
    let Some(mode) = &launch.mode else {
        return;
    };
    match agent
        .request(
            "session/set_mode",
            json!({"sessionId": agent_session_id, "modeId": mode}),
        )
        .await
    {
        Ok(_) => response["modes"]["currentModeId"] = json!(mode),
        Err(e) => {
            tracing::warn!(session = %session.id, %mode, error = %e, "switching the agent's mode")
        }
    }
}

/// Routes an ACP message from the agent: requests and notifications to the hub, responses to their caller.
pub(crate) async fn from_agent(
    hub: &Arc<Hub>,
    session_id: Uuid,
    agent: &Arc<AgentConn>,
    message: Value,
) {
    if message.get("method").is_some() {
        if message.get("id").is_some() {
            hub.on_agent_request(session_id, agent, message).await;
        } else {
            hub.on_agent_notification(session_id, message).await;
        }
    } else {
        agent.handle_response(&message);
    }
}

/// How long the link may be silent before it counts as dead; the supervisor pings every 20 s.
const LINK_TIMEOUT: Duration = Duration::from_secs(60);

/// The supervisor's first message: its version, and the agent it kept running if there is one.
async fn hello(
    stream: &mut futures::stream::SplitStream<WebSocket>,
) -> Option<(String, Option<RunningAgent>, bool, bool)> {
    loop {
        let message = tokio::time::timeout(LINK_TIMEOUT, stream.next())
            .await
            .ok()??
            .ok()?;
        match message {
            Message::Text(text) => {
                return match serde_json::from_str(&text).ok()? {
                    FromSupervisor::Hello {
                        version,
                        agent,
                        local_proxy,
                        fetch_transcript,
                    } => Some((version, agent, local_proxy, fetch_transcript)),
                    _ => None,
                };
            }
            Message::Close(_) => return None,
            _ => {}
        }
    }
}

#[cfg(test)]
mod shutdown_tests {
    use sloppod_protocol::link::RunningProcess;

    use super::*;

    #[test]
    fn the_agent_hears_what_its_pod_cut_off() {
        let notice = shutdown_notice(&[
            RunningProcess {
                command: "bash -c cargo test".into(),
                output_tail: Some("test a ... ok".into()),
            },
            RunningProcess {
                command: "bash -c npm run dev".into(),
                output_tail: None,
            },
        ]);
        assert!(notice.starts_with("sloppod: your session's pod was stopped"));
        assert!(
            notice.contains("\n- `bash -c cargo test`, which last wrote:\n```\ntest a ... ok\n```")
        );
        assert!(notice.ends_with("\n- `bash -c npm run dev`"));
    }
}
