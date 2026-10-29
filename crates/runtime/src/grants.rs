//! What each grant lets a session do, in the words both its owner and its agent see.

use serde_json::{Value, json};
use sloppod_contracts::capability;
use sloppod_contracts::{Grant, READ_ONLY, READ_WRITE, operation};

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

/// The accounts a model grant names, in the order they're used (ADR 29), if it names any.
pub fn accounts(grant: &Grant) -> Option<Vec<&str>> {
    let ids = grant.params.get("credentials")?.as_array()?;
    Some(ids.iter().filter_map(Value::as_str).collect())
}

/// The accounts the owner gives a model grant, in order, checked (ADR 29): at least one, each one of theirs of the
/// grant's kind, none twice. The first is the one it uses; putting another first is ADR 20's account switch.
pub async fn accounts_given(
    credentials: &crate::credentials::Credentials,
    owner: &str,
    grant: &Grant,
    ids: &[String],
) -> sqlx::Result<Result<Vec<crate::credentials::CredentialRow>, String>> {
    let Some(kind) = crate::capabilities::account_kind(&grant.capability) else {
        return Ok(Err(format!(
            "a {} grant doesn't name accounts",
            grant.capability
        )));
    };
    if ids.is_empty() {
        return Ok(Err("a model grant names at least one account".into()));
    }
    let mut accounts: Vec<crate::credentials::CredentialRow> = Vec::new();
    for id in ids {
        let Some(account) = credentials.owned(owner, kind, id).await? else {
            return Ok(Err(format!("{id} isn't one of your {kind} credentials")));
        };
        if accounts.iter().any(|a| a.id == account.id) {
            return Ok(Err(format!("{} is in the list twice", account.name)));
        }
        accounts.push(account);
    }
    Ok(Ok(accounts))
}

pub fn describe(grant: &Grant) -> String {
    match grant.capability.as_str() {
        capability::MODEL => "Claude's API, with the first of the owner's Claude accounts this grant names.".into(),
        capability::GITHUB => "GitHub's API and git: reads of public repositories and the ones granted below, pushes to \
                     granted session branches. Other writes go through proposals."
            .into(),
        capability::PACKAGES => "Package registries and tool downloads (crates.io, npm, PyPI, Go, GitHub releases, mise and \
                       others)."
            .into(),
        capability::CODEX => "Codex's API on chatgpt.com, with the first of the owner's ChatGPT logins this grant names.".into(),
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
            let limited = grant
                .params
                .get("operations")
                .and_then(Value::as_array)
                .is_some_and(|ops| !ops.iter().any(|o| o == operation::PROPOSE));
            match grant.param("as") {
                Some(login) if limited => format!(
                    "{access}, as {login} (the user's own GitHub token, inherited from the parent session for reading \
                     and pushing only: proposals there are the parent's)."
                ),
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

/// Why a repository grant's identity can't reach its repository, if it can't (ADR 29): the GitHub App isn't installed
/// there, or the user's token it acts with doesn't list it. Said when the grant is made, rather than left to the first
/// push or proposal to find out. A lookup that fails says nothing.
pub async fn reach_warning(
    credentials: &crate::credentials::Credentials,
    owner: &str,
    grant: &Grant,
) -> Option<String> {
    if grant.capability != capability::REPO {
        return None;
    }
    let repo = grant.param("repo")?;
    match grant.param("credential") {
        Some(credential) => match credentials.github_user(owner, credential).await {
            Ok(Some(token)) if crate::credentials::token_reaches(&token, repo).is_some() => None,
            Ok(Some(token)) => Some(format!(
                "the GitHub token {} it acts with on {repo} doesn't reach it, so git and GitHub's API there will be \
                 refused",
                token.name
            )),
            Ok(None) => Some(format!(
                "the GitHub token it acts with on {repo} is gone, so git and GitHub's API there will be refused"
            )),
            Err(_) => None,
        },
        None => match credentials.github(owner).await {
            Ok(app) => match app.installed_on(repo).await {
                Ok(false) => Some(format!(
                    "the GitHub App isn't installed on {repo}, so pushing and proposing there will fail, and so will \
                     reading it if it's private; the user can install the App there, or have the session act with one \
                     of their GitHub tokens"
                )),
                _ => None,
            },
            Err(_) => Some(format!(
                "the user has no GitHub App, so nothing on {repo} can be done as one"
            )),
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
