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
        /// The agent to run, e.g. `claude-code` (the default) or `codex`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent: Option<String>,
    }

    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum Phase {
        Starting,
        Idle,
        Working,
        Waiting,
        Disconnected,
        /// No pod; the session keeps everything else and wakes on request (ADR 5).
        Parked,
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
                Phase::Parked => "parked",
                Phase::Stopped => "stopped",
            }
        }

        pub fn parse(s: &str) -> Self {
            match s {
                "idle" => Phase::Idle,
                "working" => Phase::Working,
                "waiting" => Phase::Waiting,
                "disconnected" => Phase::Disconnected,
                "parked" => Phase::Parked,
                "stopped" => Phase::Stopped,
                _ => Phase::Starting,
            }
        }
    }

    /// An enum stored and sent as its snake_case name.
    macro_rules! named {
        ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $text:literal),+ $(,)? }) => {
            $(#[$meta])*
            #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
            #[serde(rename_all = "snake_case")]
            pub enum $name {
                $($(#[$vmeta])* $variant),+
            }

            impl $name {
                pub const ALL: &[$name] = &[$($name::$variant),+];

                pub fn as_str(self) -> &'static str {
                    match self {
                        $($name::$variant => $text),+
                    }
                }
            }

            impl std::str::FromStr for $name {
                type Err = String;

                fn from_str(s: &str) -> Result<Self, String> {
                    match s {
                        $($text => Ok($name::$variant),)+
                        _ => Err(format!("unknown {} {s}", stringify!($name))),
                    }
                }
            }

            impl TryFrom<String> for $name {
                type Error = String;

                fn try_from(s: String) -> Result<Self, String> {
                    s.parse()
                }
            }

            impl std::fmt::Display for $name {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str(self.as_str())
                }
            }
        };
    }

    named! {
        /// What a session is for.
        SessionKind {
            Work = "work",
            /// It reviews another session's proposal (ADR 24).
            Review = "review",
            /// An owner's own, for the runtime's judgements (ADR 26); its agent isn't prompted.
            Utility = "utility",
        }
    }

    named! {
        /// Whether a session's proposals are reviewed by another model before a human sees them (ADR 24).
        ReviewPolicy {
            Off = "off",
            Required = "required",
        }
    }

    named! {
        /// Where a proposal is in its life (ADR 3, ADR 24).
        ProposalStatus {
            InReview = "in_review",
            ChangesRequested = "changes_requested",
            Pending = "pending",
            Approved = "approved",
            Rejected = "rejected",
            Executed = "executed",
            Failed = "failed",
            Withdrawn = "withdrawn",
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
        pub agent: String,
        pub kind: SessionKind,
        pub review: ReviewPolicy,
        pub created_at: String,
        pub updated_at: String,
        /// When something last happened in the session; the session list is ordered by it.
        pub active_at: String,
        /// When it was deleted; it can be restored until it's purged a week later.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub deleted_at: Option<String>,
        /// What the pod is doing while the agent starts, e.g. cloning the repository.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent_status: Option<String>,
        /// Why the agent last failed to start or exited; it's started again meanwhile.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent_error: Option<String>,
        /// The session that spawned this one, for a sub-session (ADR 27).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub parent_id: Option<Uuid>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct Grant {
        pub capability: String,
        #[serde(default)]
        pub params: Value,
        /// What the grant lets the session do; set by the runtime, ignored on input.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
    }

    #[derive(Clone, Serialize, Deserialize)]
    pub struct NewCredential {
        pub kind: String,
        pub name: String,
        #[serde(default)]
        pub config: serde_json::Map<String, Value>,
        pub secret: String,
    }

    /// Leaves the secret out, so logging one can't leak it.
    impl std::fmt::Debug for NewCredential {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("NewCredential")
                .field("kind", &self.kind)
                .field("name", &self.name)
                .field("config", &self.config)
                .finish_non_exhaustive()
        }
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
        pub status: ProposalStatus,
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
        /// The session reviewing it, or that did (ADR 24).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub review_session_id: Option<Uuid>,
    }

    /// One round of a proposal's review by another model (ADR 24).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Review {
        pub round: i32,
        /// The commit reviewed.
        pub head_sha: String,
        /// The author's replies to the previous round's findings.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub replies: Option<String>,
        /// `pass` or `changes`; absent while the round runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub verdict: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub summary: Option<String>,
        /// `{id, severity, category, title, explanation, path?, line?, end_line?, suggested_fix?}`; an id carried
        /// over from an earlier round marks the same finding.
        pub findings: Value,
        /// Ids of the previous round's findings that no longer hold.
        pub resolved: Value,
        pub created_at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub finished_at: Option<String>,
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

    /// Where the supervisor serves the runtime's session endpoints (MCP, memory) to the agent, attaching the pod's
    /// ServiceAccount token to each request, so the runtime knows the session from the pod's own identity.
    pub const LOCAL_PROXY: &str = "127.0.0.1:7811";

    /// Whether `name` can be a transcript file: a plain `.jsonl` file name, nothing that reaches outside the
    /// transcript directory. Checked when the runtime stores a file and again when a supervisor restores it.
    pub fn is_transcript_file_name(name: &str) -> bool {
        name.ends_with(".jsonl")
            && name.len() > ".jsonl".len()
            && !name.starts_with('.')
            && !name.contains(['/', '\\', '\0'])
    }

    /// Path of the supervisor link WebSocket on the runtime.
    pub const PATH: &str = "/link/v1";
    /// Where a supervisor fetches a transcript file the runtime has: `GET {TRANSCRIPT_PATH}/{name}`, with the pod's
    /// token, streamed.
    pub const TRANSCRIPT_PATH: &str = "/link/v1/transcript";
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
        /// A commit to check out, detached, at every start, e.g. the one a review session reviews.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub commit: Option<String>,
        /// The branch a new session branch starts from, e.g. a sub-session's from its parent's; else the default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub from: Option<String>,
    }

    /// A transcript file the runtime has, and how many bytes of it.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct TranscriptRef {
        pub name: String,
        pub len: u64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Launch {
        pub session_id: Uuid,
        pub files: Vec<File>,
        pub transcript_dir: String,
        /// Transcript files inline, for a supervisor that doesn't fetch them.
        #[serde(default)]
        pub transcript: Vec<TranscriptFile>,
        /// The transcript files the runtime has, for the supervisor to fetch any it's missing from
        /// [`TRANSCRIPT_PATH`], and to sync only what's appended to them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub transcript_files: Vec<TranscriptRef>,
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

    /// An agent that outlived the supervisor's last link, reported when it reconnects.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RunningAgent {
        pub ready: AgentReady,
        /// Requests the runtime sent the agent that it hasn't answered.
        pub open: Vec<Value>,
        /// Requests the agent sent the runtime that weren't answered.
        pub waiting: Vec<Value>,
    }

    /// The agent's session is up; the supervisor keeps this to report the agent after a reconnect.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AgentReady {
        pub agent_session_id: String,
        /// The agent's `initialize` result.
        pub capabilities: Value,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum ToSupervisor {
        /// Start the agent, replacing any that's running.
        Launch(Box<Launch>),
        /// Carry on with the running agent.
        Attach {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            runtime_build: Option<String>,
        },
        AgentReady(AgentReady),
        Acp {
            message: Value,
        },
        SyncTranscript,
        /// Runs a command in the pod apart from the agent, in its working directory and environment with `env`
        /// changed, with `input` on its standard input: e.g. a judgement (ADR 26). Answered with `RunResult`.
        Run {
            id: u64,
            command: Vec<String>,
            #[serde(default)]
            env: Vec<(String, String)>,
            input: String,
            timeout_secs: u64,
        },
        /// The runtime's copy of a transcript file doesn't match; send all of it.
        ResyncTranscript {
            name: String,
        },
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub enum FromSupervisor {
        Hello {
            version: String,
            /// An agent still running from an earlier link; the runtime attaches to it rather than launching.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            agent: Option<RunningAgent>,
            /// The supervisor serves the runtime's session endpoints at [`LOCAL_PROXY`]; older ones don't.
            #[serde(default)]
            local_proxy: bool,
            /// The supervisor fetches transcript files it's missing rather than having them inline in `Launch`, which
            /// for a long session is one message tens of megabytes long (#211); older ones don't.
            #[serde(default)]
            fetch_transcript: bool,
        },
        Status {
            message: String,
        },
        AgentStarted,
        Acp {
            message: Value,
        },
        /// Whole transcript files (supervisors before incremental sync).
        Transcript {
            files: Vec<TranscriptFile>,
        },
        /// The bytes appended to a transcript file from `offset` on; offset 0 replaces the whole file.
        TranscriptAppend {
            name: String,
            offset: u64,
            content_b64: String,
            /// On the first piece of a replacement (offset 0) that takes more than one: the whole replacement's
            /// length. The runtime keeps its copy until all of it is in.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            replaces: Option<u64>,
        },
        AgentExited {
            code: Option<i32>,
        },
        Error {
            message: String,
        },
        /// How many of the agent's commands are running, foreground or in the background; sent when it changes.
        Commands {
            running: usize,
        },
        /// What a `Run` printed, or why it failed (it couldn't start, timed out, or exited unsuccessfully).
        RunResult {
            id: u64,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            output: Option<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            error: Option<String>,
        },
        /// The pod is being terminated, and these of the agent's commands were still running; ending the pod ends
        /// them (ADR 10, 17).
        ShutdownReport {
            running: Vec<RunningProcess>,
        },
    }

    /// A command the agent started that was still running, foreground or in the background.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct RunningProcess {
        pub command: String,
        /// The last lines it wrote, when it writes to a file, as background commands do.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub output_tail: Option<String>,
    }
}

#[cfg(test)]
mod transcript_name_tests {
    use super::link::is_transcript_file_name;

    #[test]
    fn transcript_files_stay_in_their_directory() {
        assert!(is_transcript_file_name("0b6a1c.jsonl"));
        for refused in [
            "../x.jsonl",
            "a/b.jsonl",
            "a\\b.jsonl",
            ".jsonl",
            ".hidden.jsonl",
            "x.txt",
            "",
        ] {
            assert!(!is_transcript_file_name(refused), "{refused}");
        }
    }
}
