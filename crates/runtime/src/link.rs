//! The supervisor link (ADR 17): session pods connect outward, authenticate with their projected
//! ServiceAccount token, receive their launch spec and bridge the agent's ACP stdio.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use k8s_openapi::api::authentication::v1::{TokenReview, TokenReviewSpec};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, PostParams};
use serde_json::{Value, json};
use sloppod_contracts::{AgentContext, MemoryContext};
use sloppod_protocol::link::{FromSupervisor, Launch, Repo, ToSupervisor, TranscriptFile};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::adapter::{self, PLACEHOLDER};
use crate::github::GitIdentity;
use crate::placement::WORKDIR;
use crate::pods::SESSION_LABEL;
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
}

impl AgentConn {
    fn new(tx: mpsc::UnboundedSender<ToSupervisor>) -> Self {
        Self {
            tx,
            pending: Mutex::default(),
            next_id: AtomicI64::new(1),
        }
    }

    fn send_acp(&self, message: Value) {
        let _ = self.tx.send(ToSupervisor::Acp { message });
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        self.send_acp(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.await
            .unwrap_or_else(|_| Err(RpcError::new(-32002, "the session's agent disconnected")))
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

    fn handle_response(&self, message: &Value) {
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

    /// Fails every outstanding request; the link is gone.
    pub fn close(&self) {
        self.pending.lock().expect("pending lock").clear();
    }
}

pub async fn handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let Some(token) = token else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };
    let session = match authenticate(&state, &token).await {
        Ok(session) => session,
        Err(e) => {
            tracing::warn!(error = %e, "supervisor authentication failed");
            return (StatusCode::FORBIDDEN, e.to_string()).into_response();
        }
    };
    ws.max_message_size(256 << 20)
        .on_upgrade(move |socket| run(state, session, socket))
}

async fn authenticate(state: &AppState, token: &str) -> anyhow::Result<Uuid> {
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
) -> anyhow::Result<(Launch, sloppod_contracts::AgentLaunch)> {
    let agent = state.adapter.launch(context);
    let transcript = db::transcripts(&state.db, session.id)
        .await?
        .into_iter()
        .map(|(name, content)| TranscriptFile {
            name,
            content_b64: base64::engine::general_purpose::STANDARD.encode(content),
        })
        .collect();
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
        ca_pem: Some(state.ca.cert_pem.clone()),
        repo: session
            .repo
            .as_deref()
            .and_then(|repo| context.session.writable_repo_for(repo))
            .map(|grant| Repo {
                clone_url: format!("https://github.com/{}.git", grant.repo),
                branch: grant.branch.to_owned(),
                path: REPO_DIR.to_owned(),
            }),
        install_tools: session.repo.is_some(),
        command: agent.command.clone(),
        env,
        cwd: work_dir(session).to_owned(),
    };
    Ok((launch, agent))
}

pub async fn agent_context(state: &AppState, session: &db::SessionRow) -> AgentContext {
    let context = session.context();
    let namespace = context
        .grant("memory")
        .and_then(|g| g.param("namespace"))
        .map(str::to_owned);
    let memory = match namespace {
        Some(namespace) => match state.credentials.memini(&session.owner).await {
            Ok(Some(_)) => Some(MemoryContext {
                base_url: format!("{}{}", state.config.runtime_url(), crate::memory::PREFIX),
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
        tools_url: format!("{}/mcp", state.config.runtime_url()),
        instructions: state.instructions.clone(),
        model: session.model.clone(),
        memory,
        runtime_build: state.config.build.clone(),
    }
}

async fn run(state: AppState, session_id: Uuid, socket: WebSocket) {
    let Ok(Some(session)) = db::get_session(&state.db, session_id).await else {
        tracing::warn!(session = %session_id, "link from unknown session");
        return;
    };
    if session.phase() == sloppod_protocol::api::Phase::Stopped {
        return;
    }
    tracing::info!(session = %session_id, "supervisor connected");
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
    let agent = Arc::new(AgentConn::new(tx.clone()));
    state.hub.attach_agent(session_id, agent.clone()).await;
    state
        .hub
        .set_phase(session_id, sloppod_protocol::api::Phase::Starting)
        .await;

    let context = agent_context(&state, &session).await;
    let (launch, agent_launch) = match launch_for(&state, &session, &context).await {
        Ok(launch) => launch,
        Err(e) => {
            tracing::error!(session = %session_id, error = %e, "building launch spec");
            return;
        }
    };
    let _ = tx.send(ToSupervisor::Launch(Box::new(launch)));

    while let Some(Ok(message)) = stream.next().await {
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
            FromSupervisor::Hello { version } => {
                tracing::info!(session = %session_id, %version, "supervisor hello");
            }
            FromSupervisor::Status { message } => {
                tracing::info!(session = %session_id, %message, "supervisor status");
            }
            FromSupervisor::AgentStarted => {
                let (state, agent, session, agent_launch) = (
                    state.clone(),
                    agent.clone(),
                    session.clone(),
                    agent_launch.clone(),
                );
                tokio::spawn(async move {
                    if let Err(e) =
                        start_agent_session(&state, &agent, &session, &agent_launch).await
                    {
                        tracing::error!(session = %session.id, error = %e, "starting agent session");
                    }
                });
            }
            FromSupervisor::Acp { message } => {
                if message.get("method").is_some() {
                    if message.get("id").is_some() {
                        state
                            .hub
                            .on_agent_request(session_id, &agent, message)
                            .await;
                    } else {
                        state.hub.on_agent_notification(session_id, message).await;
                    }
                } else {
                    agent.handle_response(&message);
                }
            }
            FromSupervisor::Transcript { files } => {
                for file in files {
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
            FromSupervisor::AgentExited { code } => {
                tracing::warn!(session = %session_id, ?code, "agent exited");
            }
            FromSupervisor::Error { message } => {
                tracing::error!(session = %session_id, %message, "supervisor error");
            }
        }
    }
    tracing::info!(session = %session_id, "supervisor disconnected");
    agent.close();
    state.hub.detach_agent(session_id, &agent).await;
    writer.abort();
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
                    auto_mode(agent, &session, existing, &mut response).await;
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
    let pending = db::take_pending_prompt(&state.db, session.id).await?;
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
    auto_mode(agent, session, &agent_session_id, &mut response).await;
    Ok((agent_session_id, response))
}

/// Sessions always run in the agent's auto mode; the sandbox and grants bound what it can do.
async fn auto_mode(
    agent: &Arc<AgentConn>,
    session: &db::SessionRow,
    agent_session_id: &str,
    response: &mut Value,
) {
    match agent
        .request(
            "session/set_mode",
            json!({"sessionId": agent_session_id, "modeId": "auto"}),
        )
        .await
    {
        Ok(_) => response["modes"]["currentModeId"] = json!("auto"),
        Err(e) => tracing::warn!(session = %session.id, error = %e, "switching to auto mode"),
    }
}
