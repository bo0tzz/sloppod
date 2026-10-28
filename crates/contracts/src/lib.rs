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

/// One capability granted to a session (ADR 2), e.g. `repo` with `{repo, access, branch}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grant {
    pub capability: String,
    #[serde(default)]
    pub params: Value,
}

impl Grant {
    pub fn new(capability: &str, params: Value) -> Self {
        Self {
            capability: capability.to_owned(),
            params,
        }
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(name).and_then(Value::as_str)
    }
}

/// What extension points know about the session they act for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContext {
    pub session_id: String,
    /// The user the session belongs to; credentials are theirs (ADR 23).
    pub owner: String,
    pub grants: Vec<Grant>,
}

/// A writable repository grant: the repository and the session branch, the only ref it may push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoGrant<'a> {
    pub repo: &'a str,
    pub branch: &'a str,
}

impl SessionContext {
    pub fn grant(&self, capability: &str) -> Option<&Grant> {
        self.grants.iter().find(|g| g.capability == capability)
    }

    pub fn has(&self, capability: &str) -> bool {
        self.grant(capability).is_some()
    }

    /// Every repository the session holds a grant for, at any access level.
    pub fn repos(&self) -> impl Iterator<Item = &str> {
        self.grants
            .iter()
            .filter(|g| g.capability == capability::REPO)
            .filter_map(|g| g.param("repo"))
    }

    /// The session's repository grants with `access`, each with its branch.
    fn repo_grants(&self, access: &str) -> impl Iterator<Item = RepoGrant<'_>> {
        self.grants
            .iter()
            .filter(move |g| g.capability == capability::REPO && g.param("access") == Some(access))
            .filter_map(|g| {
                Some(RepoGrant {
                    repo: g.param("repo")?,
                    branch: g.param("branch")?,
                })
            })
    }

    /// Repositories the session may push to, each with its session branch.
    pub fn writable_repos(&self) -> impl Iterator<Item = RepoGrant<'_>> {
        self.repo_grants(READ_WRITE)
    }

    /// The first repository the session may push to, if any.
    pub fn writable_repo(&self) -> Option<RepoGrant<'_>> {
        self.writable_repos().next()
    }

    pub fn writable_repo_for(&self, repo: &str) -> Option<RepoGrant<'_>> {
        self.writable_repos()
            .find(|g| g.repo.eq_ignore_ascii_case(repo))
    }

    /// A read-only grant of `repo` with the branch to check out, e.g. a review session's (ADR 24).
    pub fn readable_repo_for(&self, repo: &str) -> Option<RepoGrant<'_>> {
        self.repo_grants(READ_ONLY)
            .find(|g| g.repo.eq_ignore_ascii_case(repo))
    }
}

/// The built-in capability names a grant can have (ADR 2).
pub mod capability {
    /// Claude's API, with the owner's Claude account.
    pub const MODEL: &str = "model";
    /// Codex's API, with the owner's ChatGPT login.
    pub const CODEX: &str = "codex";
    /// GitHub's API and git.
    pub const GITHUB: &str = "github";
    /// Package registries and tool downloads, reached directly.
    pub const PACKAGES: &str = "packages";
    /// Long-term memory (memini).
    pub const MEMORY: &str = "memory";
    /// A repository: `{repo, access, branch?, commit?}`.
    pub const REPO: &str = "repo";
}

/// A repository grant's `access`: read only, or read and push the session's branches (ADR 2).
pub const READ_ONLY: &str = "ro";
pub const READ_WRITE: &str = "rw";

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

/// Creates and deletes session pods (ADR 16).
#[async_trait]
pub trait PlacementBackend: Send + Sync {
    fn manifest(&self) -> Manifest;
    /// Creates or updates the session's pod and volume, and runs the pod if it was suspended.
    async fn ensure(&self, spec: &PlacementSpec) -> Result<()>;
    /// Ends the session's pod and keeps its volume, for a parked session (ADR 5); `ensure` brings it back.
    async fn suspend(&self, session_id: &str) -> Result<()>;
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
    /// The repository the session is about, checked out at `repo_dir`.
    pub repo: Option<String>,
    /// Where the agent works: the checkout, or an empty directory for sessions without a repository.
    pub repo_dir: String,
    /// Where other repositories the session can push to are cloned, as `<repos_dir>/<owner>/<name>`.
    pub repos_dir: String,
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
    /// The ACP session mode to switch to after creating or loading a session.
    pub mode: Option<String>,
}

/// Starts an agent in a session pod; the agent then speaks ACP over stdio (ADR 9).
pub trait AgentAdapter: Send + Sync {
    fn manifest(&self) -> Manifest;
    /// The name sessions pick this agent by, e.g. `claude-code`.
    fn agent(&self) -> String;
    /// The credential its model needs from the session's owner.
    fn credential(&self) -> NeededCredential;
    fn launch(&self, context: &AgentContext) -> AgentLaunch;
}

/// A tool the agent gets for creating proposals of one kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// The credential an agent's model needs from a session's owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeededCredential {
    /// The credential kind, e.g. `claude`.
    pub kind: String,
    /// What to call it when it's missing, e.g. "a Claude account".
    pub name: String,
}

/// The change a reviewer looks at for a proposal (ADR 24): commits on a branch of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSubject {
    pub repo: String,
    pub base: String,
    pub head: String,
    pub head_sha: String,
    pub title: String,
    pub body: String,
}

/// An outward action the agent can propose and a human approves (ADR 3).
#[async_trait]
pub trait ProposalKind: Send + Sync {
    fn manifest(&self) -> Manifest;
    fn kind(&self) -> String;
    /// A short description of a proposal for notices, e.g. `pull request "Fix the build"`.
    fn summary(&self, payload: &Value) -> String;
    fn tool(&self) -> ToolSpec;
    /// Validates the agent's arguments and returns the payload a human reviews.
    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value>;
    /// Extra material for review, e.g. a diff.
    async fn detail(&self, session: &SessionContext, payload: &Value) -> Result<Value>;
    /// What a proposal acts on, for matching standing approvals against it, e.g. a repository as `owner/name`.
    fn target(&self, _payload: &Value) -> Option<String> {
        None
    }
    /// What a review session reviews for a proposal (ADR 24); `None` for kinds that aren't reviewed.
    fn review_subject(&self, _payload: &Value) -> Option<ReviewSubject> {
        None
    }
    /// Applies the reviewer's edits, sent along with the approval, to the payload.
    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        match edits.as_object() {
            Some(edits) if !edits.is_empty() => {
                Err(Error::new("this kind of proposal can't be edited"))
            }
            _ => Ok(payload.clone()),
        }
    }
    /// Runs the approved action with the runtime's credentials.
    async fn execute(&self, session: &SessionContext, payload: &Value) -> Result<Value>;
}
