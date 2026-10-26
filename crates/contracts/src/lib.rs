//! Extension-point contracts (ADR 7).
//!
//! Every contract is a trait whose inputs and outputs are plain serde types, so an
//! implementation can later move behind a process boundary without changing callers.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Describes an implementation of one or more extension points.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
}

impl Manifest {
    pub fn builtin(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

#[derive(Debug, thiserror::Error, Serialize, Deserialize)]
#[error("{0}")]
pub struct Error(pub String);

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// What extension points know about the session they act for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContext {
    pub session_id: String,
    /// The repository the session may write to, as `owner/name`.
    pub repo: String,
    /// The session's own branch; the only ref it may push.
    pub branch: String,
    pub memory_namespace: Option<String>,
}

/// One HTTP request leaving a session pod, as seen by the egress listener.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub host: String,
    /// Path including the query string.
    pub path: String,
    /// Header names are lowercase.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    /// The listener only forwards the start of large bodies.
    pub body_truncated: bool,
}

impl HttpRequest {
    pub fn path_only(&self) -> &str {
        self.path.split('?').next().unwrap_or_default()
    }

    pub fn query(&self) -> Option<&str> {
        self.path.split_once('?').map(|(_, q)| q)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Allow {
        set_headers: Vec<(String, String)>,
        remove_headers: Vec<String>,
    },
    Deny {
        status: u16,
        /// The response body, written so the agent knows what to do instead.
        message: String,
        /// Defaults to plain text; protocols like git need their own framing to show a message.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
    },
}

impl Decision {
    pub fn allow() -> Self {
        Self::Allow {
            set_headers: Vec::new(),
            remove_headers: Vec::new(),
        }
    }

    pub fn allow_with(set_headers: Vec<(String, String)>, remove_headers: Vec<String>) -> Self {
        Self::Allow {
            set_headers,
            remove_headers,
        }
    }

    pub fn deny(message: impl Into<String>) -> Self {
        Self::Deny {
            status: 403,
            message: message.into(),
            content_type: None,
        }
    }
}

/// Decides requests to one service and says which credential to attach (ADR 11).
#[async_trait]
pub trait RequestPolicy: Send + Sync {
    fn manifest(&self) -> Manifest;
    /// Hosts this policy decides for. Traffic to them is redirected to the listener.
    fn hosts(&self) -> Vec<String>;
    async fn decide(&self, session: &SessionContext, request: &HttpRequest) -> Result<Decision>;
}

/// A credential minted for a session, with expiry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub value: String,
    /// RFC 3339.
    pub expires_at: Option<String>,
}

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    fn manifest(&self) -> Manifest;
    async fn credential(&self, session: &SessionContext) -> Result<Credential>;
}

/// What a placement backend needs to run one session pod.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacementSpec {
    pub session_id: String,
    pub image: String,
    pub labels: BTreeMap<String, String>,
    pub env: Vec<(String, String)>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub storage_size: String,
    pub storage_class: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacementStatus {
    pub exists: bool,
}

/// Creates and deletes session pods (ADR 16).
#[async_trait]
pub trait PlacementBackend: Send + Sync {
    fn manifest(&self) -> Manifest;
    async fn ensure(&self, spec: &PlacementSpec) -> Result<()>;
    async fn status(&self, session_id: &str) -> Result<PlacementStatus>;
    async fn delete(&self, session_id: &str) -> Result<()>;
}

/// A session's egress: hosts redirected to the authorizing listener, and hosts reachable directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressSpec {
    pub session_id: String,
    pub pod_labels: BTreeMap<String, String>,
    pub intercepted_hosts: Vec<String>,
    pub direct_hosts: Vec<String>,
}

/// Applies egress to the data plane, e.g. Cilium (ADR 11).
#[async_trait]
pub trait Enforcer: Send + Sync {
    fn manifest(&self) -> Manifest;
    async fn apply(&self, egress: &EgressSpec) -> Result<()>;
    async fn remove(&self, session_id: &str) -> Result<()>;
}

/// A file the supervisor writes before starting the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentFile {
    pub path: String,
    pub content: String,
    /// Replace an existing file; otherwise only write it when missing.
    pub overwrite: bool,
}

/// Everything about a session an agent adapter may use to configure the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContext {
    pub session: SessionContext,
    pub workdir: String,
    pub repo_dir: String,
    /// The runtime's MCP endpoint for this session's tools.
    pub tools_url: String,
    pub instructions: Option<String>,
    pub model: Option<String>,
    pub memory: Option<MemoryContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryContext {
    pub base_url: String,
    pub namespace: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLaunch {
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub files: Vec<AgentFile>,
    /// Where the agent keeps its native transcript, synced as an opaque blob (ADR 1).
    pub transcript_dir: String,
    /// Params for ACP `session/new`, without `cwd`.
    pub new_session: Value,
    /// Params for ACP `session/load`, without `cwd` and `sessionId`.
    pub load_session: Value,
    /// ACP client capabilities the runtime announces.
    pub client_capabilities: Value,
}

/// Starts an agent in a session pod; the agent then speaks ACP over stdio (ADR 9).
pub trait AgentAdapter: Send + Sync {
    fn manifest(&self) -> Manifest;
    fn launch(&self, context: &AgentContext) -> AgentLaunch;
}

/// A tool the agent gets for creating proposals of one kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// An outward action the agent can propose and a human approves (ADR 3).
#[async_trait]
pub trait ProposalKind: Send + Sync {
    fn manifest(&self) -> Manifest;
    fn kind(&self) -> String;
    fn tool(&self) -> ToolSpec;
    /// Validates the agent's arguments and returns the payload a human reviews.
    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value>;
    /// Extra material for review, e.g. a diff.
    async fn detail(&self, session: &SessionContext, payload: &Value) -> Result<Value>;
    /// Runs the approved action with the runtime's credentials.
    async fn execute(&self, session: &SessionContext, payload: &Value) -> Result<Value>;
}
