//! Updating pull requests that are already open, in a repository the session can push to: commenting, closing or
//! reopening, retitling, rewriting, relabelling and marking them ready for review (ADR 3), like issue updates. Merging
//! isn't among them: that's the user's, or auto-merge's after a reviewed proposal.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use sloppod_contracts::{
    Error, Manifest, ProposalKind, RepoGrant, Result, SessionContext, ToolSpec,
};

use super::updates::{self, MAX_UPDATES};
use super::{err, target_repo};
use crate::credentials::Credentials;

pub struct PullRequestUpdates {
    credentials: Arc<Credentials>,
}

impl PullRequestUpdates {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }
}

/// What a pull request update can have.
const FIELDS: updates::Fields = updates::Fields {
    all: &[
        "number",
        "title",
        "comment",
        "body",
        "close",
        "reopen",
        "ready",
        "add_labels",
        "remove_labels",
    ],
    text: &["title", "comment", "body"],
    flags: &["close", "reopen", "ready"],
};

/// Checks a list of updates from the agent or the reviewer, keeping only what GitHub gets.
fn update_list(updates: &Value) -> Result<Vec<Value>> {
    updates::update_list(
        updates,
        "pull request",
        &FIELDS,
        |update, i, number, checked| {
            for flag in ["close", "reopen", "ready"] {
                if update[flag] == true {
                    checked.insert(flag.into(), json!(true));
                }
            }
            for (a, b) in [("close", "reopen"), ("close", "ready")] {
                if checked.contains_key(a) && checked.contains_key(b) {
                    return Err(Error::new(format!(
                        "update {} can't both {a} and {b} #{number}",
                        i + 1
                    )));
                }
            }
            Ok(())
        },
    )
}

/// What an update changes, in a few words, for the result and the agent's notice.
fn describe(update: &Value) -> String {
    let mut state = Vec::new();
    if update.get("close").is_some() {
        state.push("closed".to_owned());
    }
    if update.get("reopen").is_some() {
        state.push("reopened".to_owned());
    }
    if update.get("ready").is_some() {
        state.push("marked ready for review".to_owned());
    }
    updates::describe(update, state)
}

#[async_trait]
impl ProposalKind for PullRequestUpdates {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("github")
    }

    fn kind(&self) -> String {
        "github.pr.update".into()
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "propose_pull_request_updates".into(),
            description: "Propose updating pull requests that are already open, in a repository this session can \
                          push to: comment on them, close them (best with a comment saying why, e.g. a draft \
                          another pull request replaced), reopen them, change their title or body, add and remove \
                          labels, or mark a draft ready for review. Merging isn't among them. A human reviews them \
                          together, and can edit or drop any of them. Returns the proposal's id; check it with \
                          get_proposal."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repository as owner/name; needed when the session can push to more than one"},
                    "updates": {
                        "type": "array",
                        "maxItems": MAX_UPDATES,
                        "items": {
                            "type": "object",
                            "properties": {
                                "number": {"type": "integer", "description": "The pull request's number"},
                                "comment": {"type": "string", "description": "Markdown, posted before any other change"},
                                "close": {"type": "boolean", "description": "Close it without merging"},
                                "reopen": {"type": "boolean"},
                                "ready": {"type": "boolean", "description": "Mark a draft ready for review"},
                                "title": {"type": "string"},
                                "body": {"type": "string", "description": "Markdown; replaces the description"},
                                "add_labels": {"type": "array", "items": {"type": "string"}},
                                "remove_labels": {"type": "array", "items": {"type": "string"}},
                            },
                            "required": ["number"],
                        },
                    },
                },
                "required": ["updates"],
            }),
        }
    }

    fn target(&self, payload: &Value) -> Option<String> {
        payload["repo"].as_str().map(str::to_owned)
    }

    fn summary(&self, payload: &Value) -> String {
        let repo = payload["repo"].as_str().unwrap_or_default();
        match payload["updates"].as_array().map(Vec::as_slice) {
            Some([update]) => format!("#{} on {repo} ({})", update["number"], describe(update)),
            Some(updates) => format!("{} pull request updates on {repo}", updates.len()),
            None => format!("pull request updates on {repo}"),
        }
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let RepoGrant { repo, .. } = target_repo(session, arguments["repo"].as_str())?;
        Ok(json!({"repo": repo, "updates": update_list(&arguments["updates"])?}))
    }

    async fn detail(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let numbers = updates::rewritten(payload);
        if numbers.is_empty() {
            return Ok(json!({}));
        }
        let app = super::github_for(&self.credentials, session, repo).await?;
        let mut current = Vec::new();
        for number in numbers {
            // A pull request's title and body are its issue's.
            let mut pull = app.issue(repo, number).await.map_err(err)?;
            pull["number"] = json!(number);
            current.push(pull);
        }
        Ok(json!({"current": current}))
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        updates::edit_updates(payload, edits, update_list)
    }

    async fn execute(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let list = update_list(&payload["updates"])?;
        let app = super::github_for(&self.credentials, session, repo).await?;
        updates::execute_all(&list, "pull requests", describe, |number, update| {
            let (app, repo) = (app.clone(), repo.to_owned());
            async move { apply(&app, &repo, number, &update).await }
        })
        .await
    }
}

/// Makes one update: the comment and labels as for any update, then marks a draft ready, then the pull request's own
/// fields. Carrying out an approved proposal again (after a restart) repeats only what's harmless to repeat: a pull
/// request that's no longer a draft isn't marked again.
async fn apply(
    app: &crate::github::GitHubApp,
    repo: &str,
    number: u64,
    update: &Value,
) -> anyhow::Result<()> {
    updates::comment_and_label(app, repo, number, update).await?;
    if update.get("ready").is_some() {
        let pull = app.pull(repo, number).await?;
        if pull["draft"] == true {
            app.mark_ready(repo, pull["node_id"].as_str().unwrap_or_default())
                .await?;
        }
    }
    let mut patch = Map::new();
    for field in ["title", "body"] {
        if let Some(text) = update.get(field) {
            patch.insert(field.into(), text.clone());
        }
    }
    if update.get("close").is_some() {
        patch.insert("state".into(), json!("closed"));
    }
    if update.get("reopen").is_some() {
        patch.insert("state".into(), json!("open"));
    }
    if !patch.is_empty() {
        app.update_issue(repo, number, &Value::Object(patch))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_update_lists() {
        let list = update_list(&json!([
            {"number": 7, "comment": " Replaced by #9 ", "close": true, "remove_labels": ["wip"]},
            {"number": 8, "ready": true, "title": "Clearer", "add_labels": [" bug "]},
        ]))
        .unwrap();
        assert_eq!(
            list,
            vec![
                json!({"number": 7, "comment": "Replaced by #9", "close": true, "remove_labels": ["wip"]}),
                json!({"number": 8, "ready": true, "title": "Clearer", "add_labels": ["bug"]}),
            ]
        );
        assert!(update_list(&json!([])).is_err());
        assert!(update_list(&json!([{"comment": "no number"}])).is_err());
        assert!(update_list(&json!([{"number": 1}])).is_err());
        // False isn't a change.
        assert!(update_list(&json!([{"number": 1, "ready": false}])).is_err());
        // Nothing is dropped unseen: a field it can't have, or of the wrong type, is refused (#280). Merging isn't one
        // a pull request update can have, and a close reason is an issue's.
        assert!(update_list(&json!([{"number": 1, "merge": true}])).is_err());
        assert!(update_list(&json!([{"number": 1, "close": "completed"}])).is_err());
        assert!(update_list(&json!([{"number": 1, "ready": "yes"}])).is_err());
        assert!(update_list(&json!([{"number": 1, "title": 5}])).is_err());
        assert!(update_list(&json!(["#1"])).is_err());
        assert!(update_list(&json!([{"number": 1, "add_labels": "bug"}])).is_err());
        // An empty body is a change: it clears the body.
        assert_eq!(
            update_list(&json!([{"number": 1, "body": ""}])).unwrap(),
            vec![json!({"number": 1, "body": ""})]
        );
        assert_eq!(describe(&json!({"number": 1, "body": ""})), "body cleared");
        // What can't go together.
        assert!(update_list(&json!([{"number": 1, "close": true, "reopen": true}])).is_err());
        assert!(update_list(&json!([{"number": 1, "close": true, "ready": true}])).is_err());
        assert!(update_list(&json!([{"number": 1, "reopen": true, "ready": true}])).is_ok());
        assert!(
            update_list(&json!(vec![
                json!({"number": 1, "comment": "x"});
                MAX_UPDATES + 1
            ]))
            .is_err()
        );
    }

    #[test]
    fn edits_and_describes_updates() {
        let payload = json!({"repo": "a/one", "updates": [
            {"number": 1, "close": true, "comment": "See #2"},
            {"number": 3, "ready": true, "title": "Ready"},
        ]});
        let edited = updates::edit_updates(
            &payload,
            &json!({"updates": [{"number": 3, "ready": true}]}),
            update_list,
        )
        .unwrap();
        assert_eq!(edited["updates"], json!([{"number": 3, "ready": true}]));
        assert!(updates::edit_updates(&payload, &json!({"updates": []}), update_list).is_err());
        assert_eq!(describe(&payload["updates"][0]), "commented, closed");
        assert_eq!(
            describe(&payload["updates"][1]),
            "marked ready for review, retitled"
        );
    }
}
