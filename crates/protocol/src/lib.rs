//! Wire types for the runtime API (`docs/api.md`) and the supervisor link (ADR 17).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub mod api {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct NewSession {
        pub prompt: String,
        /// Derived from the prompt when absent; a session may have no repository at all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub repo: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub title: Option<String>,
    }

    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum Phase {
        Starting,
        Idle,
        Working,
        Waiting,
        Disconnected,
        Stopped,
    }

    impl Phase {
        pub fn as_str(self) -> &'static str {
            match self {
                Phase::Starting => "starting",
                Phase::Idle => "idle",
                Phase::Working => "working",
                Phase::Waiting => "waiting",
                Phase::Disconnected => "disconnected",
                Phase::Stopped => "stopped",
            }
        }

        pub fn parse(s: &str) -> Self {
            match s {
                "idle" => Phase::Idle,
                "working" => Phase::Working,
                "waiting" => Phase::Waiting,
                "disconnected" => Phase::Disconnected,
                "stopped" => Phase::Stopped,
                _ => Phase::Starting,
            }
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Session {
        pub id: Uuid,
        pub title: String,
        /// From the session's writable repository grant, if any.
        pub repo: Option<String>,
        pub branch: Option<String>,
        pub grants: Vec<Grant>,
        pub phase: Phase,
        pub created_at: String,
        pub updated_at: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct Grant {
        pub capability: String,
        #[serde(default)]
        pub params: Value,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct NewCredential {
        pub kind: String,
        pub name: String,
        #[serde(default)]
        pub config: serde_json::Map<String, Value>,
        pub secret: String,
    }

    /// A credential as the API shows it; the secret is write-only.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Credential {
        pub id: Uuid,
        pub kind: String,
        pub name: String,
        pub config: Value,
        pub default: bool,
        pub created_at: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Proposal {
        pub id: Uuid,
        pub session_id: Uuid,
        pub kind: String,
        pub status: String,
        pub payload: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
        pub created_at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub decided_at: Option<String>,
        /// The user who decided, or `standing-approval:<id>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub decided_by: Option<String>,
    }

    /// A pre-approved proposal pattern (ADR 3).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct StandingApproval {
        pub id: Uuid,
        pub kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub session_id: Option<Uuid>,
        pub edits: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub expires_at: Option<String>,
        pub created_at: String,
    }

    #[derive(Debug, Clone, Deserialize)]
    pub struct NewStandingApproval {
        pub kind: String,
        #[serde(default)]
        pub target: Option<String>,
        #[serde(default)]
        pub session_id: Option<Uuid>,
        #[serde(default)]
        pub edits: Option<Value>,
        /// RFC 3339.
        #[serde(default)]
        pub expires_at: Option<String>,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct Decision {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
    }
}

pub mod link {
    use super::*;

    /// Path of the supervisor link WebSocket on the runtime.
    pub const PATH: &str = "/link/v1";
    /// Where in the session's workdir the supervisor keeps `{"build": ...}` for the runtime it's connected to.
    pub const RUNTIME_INFO: &str = ".sloppod/runtime.json";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct File {
        pub path: String,
        pub content: String,
        pub overwrite: bool,
    }

    /// A transcript file, relative to the agent's transcript directory.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct TranscriptFile {
        pub name: String,
        pub content_b64: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Repo {
        pub clone_url: String,
        pub branch: String,
        pub path: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Launch {
        pub session_id: Uuid,
        pub files: Vec<File>,
        pub transcript_dir: String,
        pub transcript: Vec<TranscriptFile>,
        /// The runtime's CA, which every client in the pod must trust (ADR 11).
        pub ca_pem: Option<String>,
        pub repo: Option<Repo>,
        /// Run `mise install` in the checkout when it has a mise config.
        pub install_tools: bool,
        pub command: Vec<String>,
        pub env: Vec<(String, String)>,
        pub cwd: String,
        /// What the runtime was built from; the supervisor writes it where the agent can read it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub runtime_build: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ToSupervisor {
        Launch(Box<Launch>),
        Acp { message: Value },
        SyncTranscript,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum FromSupervisor {
        Hello { version: String },
        Status { message: String },
        AgentStarted,
        Acp { message: Value },
        Transcript { files: Vec<TranscriptFile> },
        AgentExited { code: Option<i32> },
        Error { message: String },
    }
}
