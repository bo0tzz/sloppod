//! Sub-sessions (ADR 27): a session spawns linked sessions for pieces of its work, each with its own pod and at
//! most its parent's grants.

use serde_json::{Value, json};
use sloppod_contracts::{Grant, READ_WRITE, capability, same_repo};
use sloppod_protocol::api::{Phase, SessionKind};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{AppState, db, lifecycle};

/// The most sub-sessions a tree of sessions may have running at once.
pub const MAX_RUNNING: i64 = 5;

/// Held from counting a tree's running sub-sessions to starting one, so two spawns can't both pass the count, and
/// while a deletion marks its root and lists the tree, so a spawn either lands in that list or sees the deletion.
pub static SPAWNING: Mutex<()> = Mutex::const_new(());

/// A child's name in its branch: its id's first eight characters, like a session branch's.
fn child_name(child: Uuid) -> String {
    child.simple().to_string()[..8].to_owned()
}

/// A child's grants (ADR 2's attenuation): the parent's, narrowed to the capabilities in `keep` if given. What the
/// child's agent needs to run (model access) always stays. A repository the parent may push to, the child may push
/// to on a branch named after the parent's, starting from it, so the parent can integrate and propose its work.
fn child_grants(parent: &[Grant], keep: Option<&[String]>, child: Uuid) -> Vec<Grant> {
    let kept = |capability: &str| {
        matches!(capability, capability::MODEL | capability::CODEX)
            || keep.is_none_or(|keep| keep.iter().any(|k| k == capability))
    };
    parent
        .iter()
        .filter(|grant| kept(&grant.capability))
        .map(|grant| {
            let mut grant = grant.clone();
            if grant.capability == capability::REPO {
                if let Some(branch) = grant.param("branch").map(str::to_owned) {
                    grant.params["branch"] = json!(format!("{branch}-{}", child_name(child)));
                    grant.params["from"] = json!(branch);
                }
                if let Some(params) = grant.params.as_object_mut() {
                    params.remove("commit");
                }
            }
            grant
        })
        .collect()
}

/// `spawn_session`: starts a child of `parent` on `task`.
pub async fn spawn(state: &AppState, parent: Uuid, arguments: &Value) -> Result<String, String> {
    let task = arguments["task"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("give the child its `task`")?;
    let parent = db::get_session(&state.db, parent)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("this session no longer exists")?;
    if parent.kind != SessionKind::Work {
        return Err("only work sessions can spawn sub-sessions".into());
    }
    let keep: Option<Vec<String>> = arguments["grants"].as_array().map(|grants| {
        grants
            .iter()
            .filter_map(|g| g.as_str().map(str::to_owned))
            .collect()
    });
    let id = Uuid::new_v4();
    let grants = child_grants(&parent.grants(), keep.as_deref(), id);
    // A child works where its parent can push, so the parent can integrate what it does.
    let repo = match arguments["repo"].as_str() {
        Some(wanted) => Some(
            grants
                .iter()
                .filter(|g| g.capability == capability::REPO && g.param("access") == Some(READ_WRITE))
                .filter_map(|g| g.param("repo"))
                .find(|repo| same_repo(repo, wanted))
                .map(str::to_owned)
                .ok_or_else(|| {
                    format!(
                        "a sub-session works in a repository you can push to, and you can't push to {wanted}; \
                         request it with request_repository first"
                    )
                })?,
        ),
        None => parent.repo.clone(),
    };
    let agent = arguments["agent"].as_str().unwrap_or(&parent.agent);
    let adapter = state
        .agents
        .get(agent)
        .ok_or_else(|| format!("there's no agent called {agent}"))?;
    let needed = adapter.credential();
    let has_credential = state
        .credentials
        .has_default(&parent.owner, &needed.kind)
        .await
        .map_err(|e| e.to_string())?;
    let inference = adapter.inference_capability();
    if !has_credential || !grants.iter().any(|g| g.capability == inference) {
        return Err(format!(
            "{agent} needs {} and the {inference} grant, which you can't give it",
            needed.name
        ));
    }
    let model = arguments["model"].as_str().or(parent.model.as_deref());
    let title = arguments["title"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| crate::api::default_title(task));
    let _spawning = SPAWNING.lock().await;
    if db::deleted_in_line(&state.db, parent.id)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err("this session, or one it's a sub-session of, is being deleted".into());
    }
    let running = db::running_in_tree(&state.db, parent.id)
        .await
        .map_err(|e| e.to_string())?;
    if running >= MAX_RUNNING {
        return Err(format!(
            "{running} sub-sessions of this tree are running, the most at once; wait for one to finish"
        ));
    }
    let child = lifecycle::start_session(
        state,
        id,
        lifecycle::NewSession {
            owner: &parent.owner,
            prompt: task,
            title: &title,
            custom_title: arguments["title"].is_string(),
            agent,
            model,
            review: parent.review,
            repo: repo.as_deref(),
            grants: &grants,
            parent: Some(parent.id),
        },
    )
    .await
    .map_err(|e| format!("{e:#}"))?;
    let branch = child.to_api().branch;
    Ok(json!({"id": child.id, "title": child.title, "branch": branch}).to_string())
}

/// `list_sessions`: the session's children, and how each is doing.
pub async fn list(state: &AppState, parent: Uuid) -> Result<String, String> {
    let children = db::children(&state.db, parent)
        .await
        .map_err(|e| e.to_string())?;
    let children: Vec<Value> = children
        .iter()
        .map(|child| {
            let api = child.to_api();
            json!({"id": child.id, "title": child.title, "phase": child.phase, "repo": api.repo, "branch": api.branch})
        })
        .collect();
    Ok(Value::Array(children).to_string())
}

/// `message_session`: a message from `from` to its parent or one of its children, delivered as a notice. A parked
/// sub-session is woken for it, if its tree is under the limit.
pub async fn message(state: &AppState, from: Uuid, arguments: &Value) -> Result<String, String> {
    let to: Uuid = arguments["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .ok_or("give the session's `id`")?;
    let text = arguments["text"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or("give the message's `text`")?;
    let sender = db::get_session(&state.db, from)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("this session no longer exists")?;
    let target = db::get_session(&state.db, to)
        .await
        .map_err(|e| e.to_string())?;
    let who = match &target {
        Some(target) if target.parent_id == Some(from) => "your parent session".to_owned(),
        Some(target) if sender.parent_id == Some(target.id) => {
            format!("your sub-session \"{}\" ({from})", sender.title)
        }
        _ => return Err("you can message your parent session and your own sub-sessions".into()),
    };
    let target = target.expect("matched above");
    if target.deleted_at.is_some() || target.phase() == Phase::Stopped {
        return Err("that session is stopped".into());
    }
    let _spawning = SPAWNING.lock().await;
    if target.phase() == Phase::Parked && target.parent_id.is_some() {
        let running = db::running_in_tree(&state.db, to)
            .await
            .map_err(|e| e.to_string())?;
        if running >= MAX_RUNNING {
            return Err(format!(
                "that session is parked, and waking it would make more than {MAX_RUNNING} sub-sessions of this tree \
                 run at once; wait for one to finish"
            ));
        }
    }
    state
        .hub
        .notify(to, format!("sloppod: a message from {who}: {text}"))
        .await;
    // A no-op unless it's parked, also if it parked since it was looked at.
    lifecycle::wake(state, &target)
        .await
        .map_err(|e| format!("sent, but waking it failed: {e:#}"))?;
    Ok("Sent.".into())
}

pub fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "spawn_session",
            "description": "Start a sub-session for a piece of your work that can run on its own: a session with its own pod and agent, linked to yours. It starts with your grants, or fewer, and works on a branch named after yours, starting from your branch as pushed (push first), so you can merge, rebase or propose what it does. At most 5 run at once. Returns its id and branch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "Its first message: what to do, with the context it needs; it doesn't see your conversation"},
                    "title": {"type": "string"},
                    "repo": {"type": "string", "description": "owner/name, one you have; defaults to yours"},
                    "grants": {"type": "array", "items": {"type": "string"}, "description": "The capabilities to give it, of yours (e.g. [\"github\", \"repo\", \"docs\"]); defaults to all of yours. Model access always stays"},
                    "agent": {"type": "string", "description": "claude-code or codex; defaults to yours"},
                    "model": {"type": "string", "description": "defaults to yours"},
                },
                "required": ["task"],
            },
        }),
        json!({
            "name": "message_session",
            "description": "Send a message to your parent session or one of your sub-sessions, by id. It arrives as a notice between its tool calls, and wakes it if it's parked.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "text": {"type": "string"},
                },
                "required": ["id", "text"],
            },
        }),
        json!({
            "name": "list_sessions",
            "description": "Your sub-sessions: each one's id, title, phase, repository and branch.",
            "inputSchema": {"type": "object", "properties": {}},
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(capability: &str, params: Value) -> Grant {
        Grant::new(capability, params)
    }

    #[test]
    fn a_child_gets_at_most_its_parents_grants_and_a_branch_of_its_own() {
        let parent = [
            grant(capability::MODEL, json!({})),
            grant(capability::GITHUB, json!({})),
            grant(capability::DOCS, json!({})),
            grant(
                capability::REPO,
                json!({"repo": "o/r", "access": "rw", "branch": "sloppod/abcd1234"}),
            ),
            grant(
                capability::REPO,
                json!({"repo": "o/docs", "access": "ro", "commit": "f00"}),
            ),
        ];
        let child = Uuid::parse_str("0123456789abcdef0123456789abcdef").unwrap();

        let all = child_grants(&parent, None, child);
        assert_eq!(all.len(), parent.len());
        let rw = &all[3];
        assert_eq!(rw.param("branch"), Some("sloppod/abcd1234-01234567"));
        assert_eq!(rw.param("from"), Some("sloppod/abcd1234"));
        assert_eq!(rw.param("access"), Some("rw"));
        assert_eq!(all[4].param("branch"), None);
        assert_eq!(all[4].param("commit"), None, "a child starts fresh");

        // Narrowed: only what's kept, and model access always.
        let narrow = child_grants(&parent, Some(&["docs".to_owned()]), child);
        let names: Vec<&str> = narrow.iter().map(|g| g.capability.as_str()).collect();
        assert_eq!(names, ["model", "docs"]);
        // Nothing the parent doesn't have.
        let asked = child_grants(
            &parent,
            Some(&["memory".to_owned(), "github".to_owned()]),
            child,
        );
        let names: Vec<&str> = asked.iter().map(|g| g.capability.as_str()).collect();
        assert_eq!(names, ["model", "github"]);
    }
}
