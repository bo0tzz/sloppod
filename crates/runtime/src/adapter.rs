//! Claude Code through `claude-agent-acp` (ADR 9).

use serde_json::{Value, json};
use sloppod_contracts::{AgentAdapter, AgentContext, AgentFile, AgentLaunch, Manifest, RepoGrant};
use sloppod_protocol::link::RUNTIME_INFO;

/// Credentials in the pod are placeholders; the authorizer attaches the real ones (ADR 11).
pub const PLACEHOLDER: &str = "sloppod-placeholder";
const OAUTH_PLACEHOLDER: &str = "sk-ant-oat01-sloppod-placeholder";
const MEMINI_PLUGIN: &str = "/opt/sloppod/plugins/memini";
/// Claude Code tools that don't fit a sloppod session: scheduling starts turns the runtime doesn't see and dies with
/// the pod, plan mode would leave auto mode on exit, and worktrees put work on branches the session can't push.
const DISALLOWED_TOOLS: &[&str] = &[
    "CronCreate",
    "CronDelete",
    "CronList",
    "ScheduleWakeup",
    "EnterPlanMode",
    "ExitPlanMode",
    "EnterWorktree",
    "ExitWorktree",
];

pub struct ClaudeCode;

impl ClaudeCode {
    fn session_prompt(context: &AgentContext) -> String {
        let session = &context.session;
        let dir = &context.repo_dir;
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
        workspace.push_str(
            "- In each repository, you can push only your session branch and branches named after it \
             (`<session branch>-<name>`, e.g. for a second pull request). For write access to another repository, \
             call the `request_repository` tool.\n\
             - Outward actions are proposals that a human reviews. To open a pull request, push your branch and call \
             the `propose_pull_request` tool; sloppod opens the PR once it's approved. You're told when your \
             proposals are decided. Direct writes to GitHub (gh pr create, comments, API POSTs) are refused.\n",
        );
        format!(
            "You are running in a sloppod session: a sandboxed Kubernetes pod.\n\
             {workspace}\
             - Network access is limited to the model API, GitHub, memory and package registries. Credentials are \
             attached outside the pod, so the tokens you see in the environment are placeholders; don't try to use \
             or replace them.\n\
             - Install tools through mise (`mise use`, `mise install`, `mise exec`) rather than apt or curl-to-shell.\n\
             - `{}/{RUNTIME_INFO}` says which sloppod runtime build your pod is connected to; it's rewritten whenever \
             the pod reconnects, e.g. after a deploy. Your session image only changes when your pod is recreated.",
            context.workdir
        )
    }
}

impl AgentAdapter for ClaudeCode {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("agent-claude-code")
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
                "attribution": {"pr": ""},
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

        let mcp_servers =
            json!([{"type": "http", "name": "sloppod", "url": context.tools_url, "headers": []}]);
        let meta = json!({"claudeCode": {"options": {"plugins": plugins, "disallowedTools": DISALLOWED_TOOLS}}});
        let system_prompt = json!({"mode": "append", "content": [{"type": "text", "text": Self::session_prompt(context)}]});
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
