//! The agents sessions run, each through its ACP adapter (ADR 9).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use sloppod_contracts::{
    AgentAdapter, AgentContext, AgentFile, AgentLaunch, Manifest, NeededCredential, RepoGrant,
    capability,
};
use sloppod_protocol::link::RUNTIME_INFO;

use crate::credentials::{CHATGPT, CLAUDE};

/// Credentials in the pod are placeholders; the authorizer attaches the real ones (ADR 11).
pub const PLACEHOLDER: &str = "sloppod-placeholder";
const OAUTH_PLACEHOLDER: &str = "sk-ant-oat01-sloppod-placeholder";
const MEMINI_PLUGIN: &str = "/opt/sloppod/plugins/memini";
/// Claude Code tools that don't fit a sloppod session: scheduling starts turns the runtime doesn't see and dies with
/// the pod, plan mode would leave auto mode on exit, worktrees put work on branches the session can't push, and
/// AskUserQuestion stops the turn until answered, where sloppod's `ask_user` doesn't.
const DISALLOWED_TOOLS: &[&str] = &[
    "CronCreate",
    "CronDelete",
    "CronList",
    "ScheduleWakeup",
    "EnterPlanMode",
    "ExitPlanMode",
    "EnterWorktree",
    "ExitWorktree",
    "AskUserQuestion",
];

/// The agents sessions can run, by name.
#[derive(Default, Clone)]
pub struct Agents {
    by_name: HashMap<String, Arc<dyn AgentAdapter>>,
}

impl Agents {
    pub fn register(&mut self, adapter: Arc<dyn AgentAdapter>) {
        self.by_name.insert(adapter.agent(), adapter);
    }

    pub fn get(&self, agent: &str) -> Option<&Arc<dyn AgentAdapter>> {
        self.by_name.get(agent)
    }
}

/// What every agent is told about the session it runs in.
fn session_prompt(context: &AgentContext) -> String {
    let session = &context.session;
    let dir = &context.repo_dir;
    if let Some(RepoGrant { repo, branch }) = context
        .repo
        .as_deref()
        .and_then(|r| session.readable_repo_for(r))
    {
        return format!(
            "You are running in a sloppod session: a sandboxed Kubernetes pod.\n\
             - A read-only copy of {repo} is at {dir}, with `{branch}` checked out; `git fetch` gets newer commits. \
             You can't push or propose changes.\n\
             - Network access is limited to what this session is granted. Credentials are attached outside the pod, so \
             the tokens you see in the environment are placeholders.\n\
             - Install tools through mise (`mise use`, `mise install`, `mise exec`) rather than apt or curl-to-shell."
        );
    }
    let mut workspace = match context
        .repo
        .as_deref()
        .and_then(|r| session.writable_repo_for(r))
    {
        Some(RepoGrant { repo, branch }) => format!(
            "- Your working copy of {repo} is at {dir}, on your session branch `{branch}`. Commit there and push \
             with `git push origin HEAD:{branch}`.\n"
        ),
        None => format!(
            "- This session isn't about a repository; your working directory {dir} is kept across restarts. You \
             can read GitHub and clone repositories to look at them.\n"
        ),
    };
    for RepoGrant { repo, branch } in session
        .writable_repos()
        .filter(|g| context.repo.as_deref() != Some(g.repo))
    {
        workspace.push_str(&format!(
            "- You can also push to {repo}: clone it to {}/{repo} if it isn't there yet, and push your session \
             branch `{branch}`.\n",
            context.repos_dir
        ));
    }
    workspace.push_str(&format!(
        "- Only {} survives a restart of the pod: keep your work there, and use `$TMPDIR` ({}/tmp) \
         rather than /tmp for scratch files and worktrees.\n",
        context.workdir, context.workdir
    ));
    workspace.push_str(
        "- In each repository, you can push only your session branch and branches named after it \
         (`<session branch>-<name>`, e.g. for a second pull request; the name has only letters, digits, `.`, `_` \
         and `-`). For write access to another repository, call the `request_repository` tool.\n",
    );
    format!(
        "You are running in a sloppod session: a sandboxed Kubernetes pod.\n\
         \n\
         # Your environment\n\
         {workspace}\
         - Network access is limited to what this session is granted; the `list_grants` tool lists it. Other hosts \
         don't resolve, so a DNS lookup failure (e.g. `EAI_FAIL`, `ENOTFOUND`) means the host is blocked, not down; \
         don't retry it. Credentials are attached outside the pod, so the tokens you see in the environment are \
         placeholders; don't try to use or replace them.\n\
         - Install tools through mise (`mise use`, `mise install`, `mise exec`) rather than apt or curl-to-shell.\n\
         - `{workdir}/{RUNTIME_INFO}` says which sloppod runtime build your pod is connected to; it's rewritten \
         whenever the pod reconnects, e.g. after a deploy. Your session image only changes when your pod is \
         recreated.\n\
         \n\
         # Acting outside the pod\n\
         - Outward actions are proposals that a human reviews. To open a pull request, push your branch and call \
         the `propose_pull_request` tool; sloppod opens the PR once it's approved. To file issues, call the \
         `propose_issues` tool. For changes that build on each other, manage the branches with `gh stack` \
         (`init`, `add`, `rebase`, `push`) and propose them together with the `propose_stack` tool rather than \
         `gh stack submit`. Direct writes to GitHub (gh pr create, gh issue create, comments, API POSTs) are \
         refused.\n\
         - Messages that start with `sloppod:` come from the runtime, not the user: proposals decided, pull requests \
         that failed their checks, conflict, got reviews or merged, answers to your questions. They arrive by \
         themselves, so don't poll for any of them.\n\
         \n\
         # Working asynchronously\n\
         The user isn't watching every turn. They read your messages when they come back, possibly hours later.\n\
         - Nothing you propose or ask blocks you. Proposals are reviewed and answers given in their own time. Carry \
         on with work that doesn't depend on them, and say what you're waiting on when you stop.\n\
         - To ask the user something, use the `ask_user` tool. It doesn't wait, and the answer arrives later as a \
         message. Use it for decisions you can't make yourself, not to report progress or to ask permission for what \
         the task already covers. Your own recommendation isn't the user's decision until they've answered.\n\
         - Run long commands (builds, test suites) in the background and keep working. Don't end your turn \
         claiming work is running unless it is.\n\
         - Before saying something waits on the user, check it still does: look up the pull request's state. Things \
         get merged fast.\n\
         - Keep updates short: what changed, what waits on the user, with links to act on. Leave out what they \
         already know.\n\
         \n\
         # Delegating\n\
         - Your built-in subagents share this pod, its grants and its lifetime. Use them only for small, quick \
         things: a search across many files, a lookup, a small isolated edit.\n\
         - Independent work that can run in parallel goes to a sub-session (`spawn_session`). It has its own pod, \
         branch and transcript, and reports back with `report_done`.\n\
         - A review by another model goes through `request_review` (or `review: true` on a proposal), not a \
         subagent you ask to review your own work.\n\
         \n\
         # Feedback on sloppod\n\
         You're one of sloppod's users, and the people building it want to know how it works for you. When \
         something in sloppod gets in your way, tell the user briefly, with what you tried. That includes a confusing \
         tool, a refusal you didn't expect, a missing grant, or a message you couldn't act on. Mention it once, \
         alongside your work, rather than stopping for it.",
        workdir = context.workdir
    )
}

pub struct ClaudeCode;

impl AgentAdapter for ClaudeCode {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("agent-claude-code")
    }

    fn agent(&self) -> String {
        "claude-code".into()
    }

    fn credential(&self) -> NeededCredential {
        NeededCredential {
            kind: CLAUDE.into(),
            name: "a Claude account".into(),
        }
    }

    fn inference_capability(&self) -> String {
        capability::MODEL.into()
    }

    fn launch(&self, context: &AgentContext) -> AgentLaunch {
        let session_id = &context.session.session_id;
        let config_dir = format!("{}/.claude", context.workdir);
        let mut env = vec![
            ("CLAUDE_CONFIG_DIR".to_owned(), config_dir.clone()),
            (
                "CLAUDE_CODE_PROJECT_DIR_NAME".to_owned(),
                session_id.clone(),
            ),
            (
                "CLAUDE_CODE_OAUTH_TOKEN".to_owned(),
                OAUTH_PLACEHOLDER.to_owned(),
            ),
            (
                "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_owned(),
                "1".to_owned(),
            ),
            ("CLAUDE_CODE_DISABLE_AUTO_MEMORY".to_owned(), "1".to_owned()),
            (
                "CLAUDE_CODE_AUTO_BACKGROUND_TIMEOUT_MS".to_owned(),
                "15000".to_owned(),
            ),
            ("DISABLE_AUTOUPDATER".to_owned(), "1".to_owned()),
        ];
        if let Some(model) = &context.model {
            env.push(("ANTHROPIC_MODEL".to_owned(), model.clone()));
        }
        let mut plugins = Vec::new();
        if let Some(memory) = &context.memory {
            env.extend([
                (
                    "MEMINI_BASE_URL".to_owned(),
                    memory.base_url.trim_end_matches('/').to_owned(),
                ),
                ("MEMINI_API_KEY".to_owned(), PLACEHOLDER.to_owned()),
                ("MEMINI_NAMESPACE".to_owned(), memory.namespace.clone()),
            ]);
            plugins.push(json!({"type": "local", "path": MEMINI_PLUGIN}));
        }

        let mut files = vec![AgentFile {
            path: format!("{config_dir}/settings.json"),
            content: serde_json::to_string_pretty(&json!({
                "autoMemoryEnabled": false,
                "attribution": {"commit": "", "pr": ""},
                // The authorizer only lets pushes through to the session's own branches, so force pushes there
                // are the agent's call; without this, auto mode blocks them as destructive.
                "permissions": {"allow": ["Bash(git push:*)"]},
            }))
            .unwrap_or_default(),
            overwrite: true,
        }];
        if let Some(instructions) = &context.instructions {
            files.push(AgentFile {
                path: format!("{config_dir}/CLAUDE.md"),
                content: instructions.clone(),
                overwrite: true,
            });
        }

        let mcp_servers = mcp_servers(context);
        let meta = json!({"claudeCode": {"options": {"plugins": plugins, "disallowedTools": DISALLOWED_TOOLS}}});
        let system_prompt = json!({"mode": "append", "content": [{"type": "text", "text": session_prompt(context)}]});
        let session_params =
            json!({"mcpServers": mcp_servers, "systemPrompt": system_prompt, "_meta": meta});

        AgentLaunch {
            command: vec!["claude-agent-acp".to_owned()],
            env,
            files,
            transcript_dir: format!("{config_dir}/projects/{session_id}"),
            new_session: session_params.clone(),
            load_session: session_params,
            client_capabilities: json!({
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false,
                "session": {"compaction": {}},
                // Lets the agent ask the user questions (AskUserQuestion) as form elicitations.
                "elicitation": {"form": {}},
            }),
            // Sessions always run in the agent's auto mode; the sandbox and grants bound what it can do.
            mode: Some("auto".into()),
        }
    }
}

/// The MCP servers every agent gets: sloppod's own tools, and the docs server if the session may use it.
fn mcp_servers(context: &AgentContext) -> Value {
    let mut servers =
        vec![json!({"type": "http", "name": "sloppod", "url": context.tools_url, "headers": []})];
    if context.session.has(capability::DOCS) {
        servers.push(json!({"type": "http", "name": "docs", "url": crate::policies::docs::URL, "headers": []}));
    }
    Value::Array(servers)
}

/// Codex through `codex-acp`, on the owner's ChatGPT subscription (ADR 24).
pub struct Codex;

/// Codex's backend for ChatGPT logins; the authorizer swaps the placeholder token for the owner's.
const CODEX_CONFIG: &str = r#"model_provider = "sloppod"

[model_providers.sloppod]
name = "ChatGPT"
base_url = "https://chatgpt.com/backend-api/codex"
wire_api = "responses"
experimental_bearer_token = "sloppod-placeholder"
"#;
/// No sandbox or approvals inside the pod: like Claude Code's auto mode, the pod and its grants are the boundary.
const CODEX_MODE: &str = "agent-full-access";

impl AgentAdapter for Codex {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("agent-codex")
    }

    fn agent(&self) -> String {
        "codex".into()
    }

    fn credential(&self) -> NeededCredential {
        NeededCredential {
            kind: CHATGPT.into(),
            name: "a ChatGPT login".into(),
        }
    }

    fn inference_capability(&self) -> String {
        capability::CODEX.into()
    }

    fn launch(&self, context: &AgentContext) -> AgentLaunch {
        let home = format!("{}/.codex", context.workdir);
        let mut instructions = session_prompt(context);
        if let Some(extra) = &context.instructions {
            instructions = format!("{extra}\n\n{instructions}");
        }
        let files = vec![
            AgentFile {
                path: format!("{home}/config.toml"),
                content: CODEX_CONFIG.to_owned(),
                overwrite: true,
            },
            AgentFile {
                path: format!("{home}/AGENTS.md"),
                content: instructions,
                overwrite: true,
            },
        ];
        let session_params = json!({"mcpServers": mcp_servers(context)});
        AgentLaunch {
            command: vec!["codex-acp".to_owned()],
            env: vec![
                ("CODEX_HOME".to_owned(), home.clone()),
                ("INITIAL_AGENT_MODE".to_owned(), CODEX_MODE.to_owned()),
                ("NO_BROWSER".to_owned(), "1".to_owned()),
            ],
            files,
            // Codex keeps its threads in SQLite under its home, on the session volume, not as transcript files.
            transcript_dir: format!("{home}/sessions"),
            new_session: session_params.clone(),
            load_session: session_params,
            client_capabilities: json!({
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false,
            }),
            mode: Some(CODEX_MODE.into()),
        }
    }
}

/// Strips the parts of an agent's `session/new` or `session/load` response worth keeping for UIs.
pub fn agent_state(response: &Value) -> Value {
    let mut state = serde_json::Map::new();
    for key in ["modes", "models", "configOptions"] {
        if let Some(value) = response.get(key) {
            state.insert(key.to_owned(), value.clone());
        }
    }
    Value::Object(state)
}

#[cfg(test)]
mod tests {
    use sloppod_contracts::SessionContext;

    use super::*;

    fn context() -> AgentContext {
        AgentContext {
            session: SessionContext {
                session_id: "s".into(),
                owner: "dev".into(),
                grants: vec![],
            },
            workdir: "/workspace".into(),
            repo: None,
            repo_dir: "/workspace/repo".into(),
            repos_dir: "/workspace/repos".into(),
            tools_url: "http://runtime/mcp".into(),
            instructions: Some("Be brief.".into()),
            model: Some("claude-haiku".into()),
            memory: None,
        }
    }

    #[test]
    fn agents_get_the_docs_server_with_the_grant() {
        let names = |context: &AgentContext| {
            mcp_servers(context)
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let mut context = context();
        assert_eq!(names(&context), ["sloppod"]);
        context
            .session
            .grants
            .push(sloppod_contracts::Grant::new(capability::DOCS, json!({})));
        assert_eq!(names(&context), ["sloppod", "docs"]);
        // Both agents get the same servers.
        assert_eq!(
            ClaudeCode.launch(&context).new_session["mcpServers"],
            Codex.launch(&context).new_session["mcpServers"]
        );
    }

    #[test]
    fn codex_talks_to_chatgpt_through_the_authorizer() {
        let launch = Codex.launch(&context());
        let config = launch
            .files
            .iter()
            .find(|f| f.path == "/workspace/.codex/config.toml")
            .unwrap();
        assert!(
            config
                .content
                .contains("base_url = \"https://chatgpt.com/backend-api/codex\"\n")
        );
        assert!(
            config
                .content
                .contains(&format!("experimental_bearer_token = \"{PLACEHOLDER}\"\n"))
        );
        // The session's model is a Claude one.
        assert!(!config.content.contains("model ="));

        let agents_md = launch
            .files
            .iter()
            .find(|f| f.path.ends_with("AGENTS.md"))
            .unwrap();
        assert!(
            agents_md
                .content
                .starts_with("Be brief.\n\nYou are running in a sloppod session")
        );
        assert_eq!(
            launch.new_session["mcpServers"][0]["url"],
            "http://runtime/mcp"
        );
        assert_eq!(launch.mode.as_deref(), Some(CODEX_MODE));
        assert!(
            launch
                .env
                .contains(&("CODEX_HOME".into(), "/workspace/.codex".into()))
        );
    }
}
