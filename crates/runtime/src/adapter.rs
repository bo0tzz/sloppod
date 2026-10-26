//! Claude Code through `claude-agent-acp` (ADR 9).

use serde_json::{Value, json};
use sloppod_contracts::{AgentAdapter, AgentContext, AgentFile, AgentLaunch, Manifest};

/// Credentials in the pod are placeholders; the authorizer attaches the real ones (ADR 11).
pub const PLACEHOLDER: &str = "sloppod-placeholder";
const OAUTH_PLACEHOLDER: &str = "sk-ant-oat01-sloppod-placeholder";
const MEMINI_PLUGIN: &str = "/opt/sloppod/plugins/memini";

pub struct ClaudeCode;

impl ClaudeCode {
    fn session_prompt(context: &AgentContext) -> String {
        let session = &context.session;
        format!(
            "You are running in a sloppod session: a sandboxed Kubernetes pod.\n\
             - Your working copy of {repo} is at {dir}, on your session branch `{branch}`. Commit there and push with \
             `git push origin HEAD:{branch}`; that is the only ref you can push.\n\
             - Outward actions are proposals that a human reviews. To open a pull request, push your branch and \
             call the `propose_pull_request` tool; sloppod opens the PR once it's approved. Direct writes to GitHub \
             (gh pr create, comments, API POSTs) are refused.\n\
             - Network access is limited to the model API, GitHub, memory and package registries. Credentials are \
             attached outside the pod, so the tokens you see in the environment are placeholders; don't try to use \
             or replace them.\n\
             - Install tools through the repository's mise config (`mise install`, `mise exec`) rather than apt or \
             curl-to-shell.",
            repo = session.repo,
            dir = context.repo_dir,
            branch = session.branch,
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
            content: serde_json::to_string_pretty(&json!({"autoMemoryEnabled": false}))
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
        let meta = json!({"claudeCode": {"options": {"plugins": plugins}}});
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
