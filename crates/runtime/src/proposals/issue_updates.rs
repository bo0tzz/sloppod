//! Updating existing GitHub issues in a repository the session can push to: commenting, closing or reopening,
//! retitling, rewriting and relabelling them (ADR 3), so sessions can keep a tracker current.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use sloppod_contracts::{
    Error, Manifest, ProposalKind, RepoGrant, Result, SessionContext, ToolSpec,
};

use super::updates::{self, MAX_UPDATES};
use super::{err, target_repo};
use crate::credentials::Credentials;

pub struct IssueUpdates {
    credentials: Arc<Credentials>,
}

impl IssueUpdates {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }
}

/// Why an issue is closed, as GitHub records it.
const CLOSE_REASONS: &[&str] = &["completed", "not_planned", "duplicate"];

/// What an issue update can have.
const FIELDS: updates::Fields = updates::Fields {
    all: &[
        "number",
        "title",
        "comment",
        "body",
        "close",
        "reopen",
        "add_labels",
        "remove_labels",
    ],
    text: &["title", "comment", "body", "close"],
    flags: &["reopen"],
};

/// Checks a list of updates from the agent or the reviewer, keeping only what GitHub gets.
fn update_list(updates: &Value) -> Result<Vec<Value>> {
    updates::update_list(updates, "issue", &FIELDS, |update, i, number, checked| {
        match update["close"].as_str() {
            None => {}
            Some(reason) if CLOSE_REASONS.contains(&reason) => {
                checked.insert("close".into(), json!(reason));
            }
            Some(other) => {
                return Err(Error::new(format!(
                    "update {} closes as {other}; say completed, not_planned or duplicate",
                    i + 1
                )));
            }
        }
        if update["reopen"] == true {
            if checked.contains_key("close") {
                return Err(Error::new(format!(
                    "update {} can't both close and reopen #{number}",
                    i + 1
                )));
            }
            checked.insert("reopen".into(), json!(true));
        }
        Ok(())
    })
}

/// What an update changes, in a few words, for the result and the agent's notice.
fn describe(update: &Value) -> String {
    let mut state = Vec::new();
    if let Some(reason) = update["close"].as_str() {
        state.push(format!("closed as {}", reason.replace('_', " ")));
    }
    if update.get("reopen").is_some() {
        state.push("reopened".to_owned());
    }
    updates::describe(update, state)
}

#[async_trait]
impl ProposalKind for IssueUpdates {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("github")
    }

    fn kind(&self) -> String {
        "github.issue.update".into()
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "propose_issue_updates".into(),
            description: "Propose updating existing GitHub issues in a repository this session can push to: \
                          comment on them, close them (as completed, not planned or a duplicate, best with a \
                          comment saying why), reopen them, change their title or body, or add and remove labels. \
                          Use it to keep the tracker current, e.g. closing issues your work finished or merging \
                          duplicates. A human reviews them together, and can edit or drop any of them. Returns the \
                          proposal's id; check it with get_proposal."
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
                                "number": {"type": "integer"},
                                "comment": {"type": "string", "description": "Markdown, posted before any other change"},
                                "close": {"type": "string", "enum": CLOSE_REASONS},
                                "reopen": {"type": "boolean"},
                                "title": {"type": "string"},
                                "body": {"type": "string", "description": "Markdown; replaces the body"},
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
            Some(updates) => format!("{} issue updates on {repo}", updates.len()),
            None => format!("issue updates on {repo}"),
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
            let mut issue = app.issue(repo, number).await.map_err(err)?;
            issue["number"] = json!(number);
            current.push(issue);
        }
        Ok(json!({"current": current}))
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        updates::edit_updates(payload, edits, update_list)
    }

    async fn execute(
        &self,
        session: &SessionContext,
        proposal: uuid::Uuid,
        payload: &Value,
    ) -> Result<Value> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let updates = update_list(&payload["updates"])?;
        let app = super::github_for(&self.credentials, session, repo).await?;

        updates::execute_all(&updates, "issues", describe, |item, number, update| {
            let (app, repo) = (app.clone(), repo.to_owned());
            let marker = super::marker(proposal, item);
            async move { apply(&app, &repo, number, &update, &marker).await }
        })
        .await
    }
}

/// Makes one update: the comment and labels as for any update, then the issue's own fields.
async fn apply(
    app: &crate::github::GitHubApp,
    repo: &str,
    number: u64,
    update: &Value,
    marker: &str,
) -> anyhow::Result<()> {
    updates::comment_and_label(app, repo, number, update, marker).await?;
    let mut patch = Map::new();
    for field in ["title", "body"] {
        if let Some(text) = update.get(field) {
            patch.insert(field.into(), text.clone());
        }
    }
    if let Some(reason) = update["close"].as_str() {
        patch.insert("state".into(), json!("closed"));
        patch.insert("state_reason".into(), json!(reason));
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
        let updates = update_list(&json!([
            {"number": 7, "comment": " Done in #12 ", "close": "completed", "remove_labels": ["priority"]},
            {"number": 8, "add_labels": [" bug "], "title": "Clearer"},
        ]))
        .unwrap();
        assert_eq!(
            updates,
            vec![
                json!({"number": 7, "comment": "Done in #12", "close": "completed", "remove_labels": ["priority"]}),
                json!({"number": 8, "add_labels": ["bug"], "title": "Clearer"}),
            ]
        );
        assert!(update_list(&json!([])).is_err());
        assert!(update_list(&json!([{"comment": "no number"}])).is_err());
        assert!(update_list(&json!([{"number": 1}])).is_err());
        // Nothing is dropped unseen: a field it can't have, or of the wrong type, is refused (#280).
        assert!(update_list(&json!([{"number": 1, "comment": "Done.", "close": true}])).is_err());
        assert!(
            update_list(&json!([{"number": 1, "comment": "Done.", "closed": "completed"}]))
                .is_err()
        );
        assert!(update_list(&json!([{"number": 1, "comment": "Done.", "reopen": "yes"}])).is_err());
        assert!(update_list(&json!([{"number": 1, "title": 5}])).is_err());
        assert!(update_list(&json!(["#1"])).is_err());
        // An empty body is a change: it clears the body.
        assert_eq!(
            update_list(&json!([{"number": 1, "body": ""}])).unwrap(),
            vec![json!({"number": 1, "body": ""})]
        );
        assert_eq!(describe(&json!({"number": 1, "body": ""})), "body cleared");
        assert!(update_list(&json!([{"number": 1, "close": "wontfix"}])).is_err());
        assert!(
            update_list(&json!([{"number": 1, "close": "completed", "reopen": true}])).is_err()
        );
        assert!(update_list(&json!([{"number": 1, "add_labels": "bug"}])).is_err());
        assert!(
            update_list(&json!(vec![
                json!({"number": 1, "comment": "x"});
                MAX_UPDATES + 1
            ]))
            .is_err()
        );
    }

    #[test]
    fn finds_the_issues_whose_text_is_replaced() {
        let payload = json!({"repo": "a/one", "updates": [
            {"number": 9, "body": "New"},
            {"number": 3, "comment": "Only a comment"},
            {"number": 4, "title": "Clearer"},
            {"number": 9, "body": ""},
        ]});
        assert_eq!(updates::rewritten(&payload), [4, 9]);
        assert!(
            updates::rewritten(&json!({"updates": [{"number": 1, "close": "completed"}]}))
                .is_empty()
        );
    }

    #[test]
    fn edits_and_describes_updates() {
        let payload = json!({"repo": "a/one", "updates": [{"number": 1, "close": "duplicate", "comment": "See #2"}, {"number": 3, "reopen": true}]});
        let edited = updates::edit_updates(
            &payload,
            &json!({"updates": [{"number": 3, "reopen": true}]}),
            update_list,
        )
        .unwrap();
        assert_eq!(edited["updates"], json!([{"number": 3, "reopen": true}]));
        assert!(updates::edit_updates(&payload, &json!({"updates": []}), update_list).is_err());
        assert_eq!(
            describe(&payload["updates"][0]),
            "commented, closed as duplicate"
        );
    }
}
