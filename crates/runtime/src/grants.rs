//! What each grant lets a session do, in the words both its owner and its agent see.

use serde_json::{Value, json};
use sloppod_contracts::capability;
use sloppod_contracts::{Grant, READ_ONLY, READ_WRITE};

/// The built-ins the owner can grant a session mid-session from its grants list (`capabilities::BUILTINS`).
pub fn grantable(capability: &str) -> bool {
    crate::capabilities::builtin(capability).is_some_and(|b| b.grantable)
}

/// A narrower version of a grant, if it has one: a repository the session can push to becomes read only.
pub fn narrowed(grant: &Grant) -> Option<Value> {
    if grant.capability != capability::REPO || grant.param("access") != Some(READ_WRITE) {
        return None;
    }
    let mut params = grant.params.clone();
    let fields = params.as_object_mut()?;
    fields.insert("access".into(), json!(READ_ONLY));
    fields.remove("branch");
    Some(params)
}

pub fn describe(grant: &Grant) -> String {
    match grant.capability.as_str() {
        capability::MODEL => "Claude's API, with the owner's Claude account.".into(),
        capability::GITHUB => "GitHub's API and git: reads of public repositories and the ones granted below, pushes to \
                     granted session branches. Other writes go through proposals."
            .into(),
        capability::PACKAGES => "Package registries and tool downloads (crates.io, npm, PyPI, Go, GitHub releases, mise and \
                       others)."
            .into(),
        capability::CODEX => "Codex's API on chatgpt.com, with the owner's ChatGPT login.".into(),
        capability::DOCS => "Library documentation, searched by library and topic, through Context7's MCP server (the `docs` \
                   tools)."
            .into(),
        capability::MEMORY => match grant.param("namespace") {
            Some(namespace) => format!("Long-term memory (memini), in the namespace {namespace}."),
            None => "Long-term memory (memini).".into(),
        },
        capability::REPO => {
            let repo = grant.param("repo").unwrap_or("?");
            let access = match (grant.param("access"), grant.param("branch")) {
                (Some(READ_WRITE), Some(branch)) => format!(
                    "{repo}: read, and push the session branch `{branch}` and branches named after it"
                ),
                _ => format!("{repo}: read only"),
            };
            match grant.param("as") {
                Some(login) => format!("{access}, as {login} (the user's own GitHub token)."),
                None => format!("{access}."),
            }
        }
        capability::WORK => format!(
            "The work graph {} ({}): {}.",
            grant.param("name").unwrap_or("?"),
            grant.param("graph").unwrap_or("?"),
            match grant.param("level") {
                Some("plan") => "read it, track work in it, and propose plan changes (ranking, reparenting, dropping)",
                Some("track") => "read it and track work in it: add items and edges, link work, move items between open, active and done",
                _ => "read it",
            }
        ),
        // A capability the owner defined says what it is itself.
        other => match grant.param("description") {
            Some(description) => description.to_owned(),
            None => format!("{other}."),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_writable_repository_narrows() {
        let rw = Grant::new(
            "repo",
            json!({"repo": "a/b", "access": "rw", "branch": "sloppod/1"}),
        );
        assert_eq!(narrowed(&rw), Some(json!({"repo": "a/b", "access": "ro"})));
        let ro = Grant::new("repo", json!({"repo": "a/b", "access": "ro"}));
        assert_eq!(narrowed(&ro), None);
        assert_eq!(narrowed(&Grant::new("github", json!({}))), None);
        assert!(grantable("packages") && !grantable("memory") && !grantable("repo"));
    }

    #[test]
    fn repo_grants_name_their_branch() {
        let rw = Grant::new(
            "repo",
            json!({"repo": "a/b", "access": "rw", "branch": "sloppod/1"}),
        );
        assert_eq!(
            describe(&rw),
            "a/b: read, and push the session branch `sloppod/1` and branches named after it."
        );
        let ro = Grant::new(capability::REPO, json!({"repo": "a/b", "access": "ro"}));
        assert_eq!(describe(&ro), "a/b: read only.");
    }
}
