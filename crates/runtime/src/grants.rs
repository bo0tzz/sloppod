//! What each grant lets a session do, in the words both its owner and its agent see.

use sloppod_contracts::capability;
use sloppod_contracts::{Grant, READ_WRITE};

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
        capability::MEMORY => match grant.param("namespace") {
            Some(namespace) => format!("Long-term memory (memini), in the namespace {namespace}."),
            None => "Long-term memory (memini).".into(),
        },
        capability::REPO => {
            let repo = grant.param("repo").unwrap_or("?");
            match (grant.param("access"), grant.param("branch")) {
                (Some(READ_WRITE), Some(branch)) => format!(
                    "{repo}: read, and push the session branch `{branch}` and branches named after it."
                ),
                _ => format!("{repo}: read only."),
            }
        }
        other => format!("{other}."),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
