//! Updating existing GitHub issues in a repository the session can push to: commenting, closing or reopening,
//! retitling, rewriting and relabelling them (ADR 3), so sessions can keep a tracker current.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use sloppod_contracts::{
    Error, Manifest, ProposalKind, RepoGrant, Result, SessionContext, ToolSpec,
};

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

const MAX_UPDATES: usize = 50;

/// Why an issue is closed, as GitHub records it.
const CLOSE_REASONS: &[&str] = &["completed", "not_planned", "duplicate"];

fn labels(update: &Value, field: &str, i: usize) -> Result<Vec<String>> {
    match &update[field] {
        Value::Null => Ok(Vec::new()),
        Value::Array(labels) => labels
            .iter()
            .map(|l| {
                l.as_str()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
            })
            .collect::<Option<_>>()
            .ok_or_else(|| Error::new(format!("update {}'s {field} must be label names", i + 1))),
        _ => Err(Error::new(format!(
            "update {}'s {field} must be a list",
            i + 1
        ))),
    }
}

/// Checks a list of updates from the agent or the reviewer, keeping only what GitHub gets. Each names an issue and
/// changes at least one thing.
fn update_list(updates: &Value) -> Result<Vec<Value>> {
    let Some(updates) = updates.as_array().filter(|u| !u.is_empty()) else {
        return Err(Error::new("give at least one update"));
    };
    if updates.len() > MAX_UPDATES {
        return Err(Error::new(format!(
            "propose at most {MAX_UPDATES} updates at a time"
        )));
    }
    updates
        .iter()
        .enumerate()
        .map(|(i, update)| {
            let number = update["number"]
                .as_u64()
                .ok_or_else(|| Error::new(format!("update {} needs the issue's number", i + 1)))?;
            let mut checked = Map::new();
            checked.insert("number".into(), json!(number));
            for field in ["title", "body", "comment"] {
                if let Some(text) = update[field].as_str().filter(|t| !t.trim().is_empty()) {
                    checked.insert(field.into(), json!(text.trim()));
                }
            }
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
            for field in ["add_labels", "remove_labels"] {
                let names = labels(update, field, i)?;
                if !names.is_empty() {
                    checked.insert(field.into(), json!(names));
                }
            }
            if checked.len() == 1 {
                return Err(Error::new(format!(
                    "update {} doesn't change #{number}",
                    i + 1
                )));
            }
            Ok(Value::Object(checked))
        })
        .collect()
}

/// The reviewer sends the updates to make, so dropping one leaves it out.
fn edit_updates(payload: &Value, edits: &Value) -> Result<Value> {
    let mut payload = payload.clone();
    if let Some(updates) = edits.get("updates") {
        payload["updates"] = json!(update_list(updates)?);
    }
    Ok(payload)
}

/// What an update changes, in a few words, for the result and the agent's notice.
fn describe(update: &Value) -> String {
    let mut parts = Vec::new();
    if update.get("comment").is_some() {
        parts.push("commented".to_owned());
    }
    if let Some(reason) = update["close"].as_str() {
        parts.push(format!("closed as {}", reason.replace('_', " ")));
    }
    if update.get("reopen").is_some() {
        parts.push("reopened".to_owned());
    }
    if update.get("title").is_some() || update.get("body").is_some() {
        parts.push("edited".to_owned());
    }
    if update.get("add_labels").is_some() || update.get("remove_labels").is_some() {
        parts.push("relabelled".to_owned());
    }
    parts.join(", ")
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

    async fn detail(&self, _session: &SessionContext, _payload: &Value) -> Result<Value> {
        Ok(json!({}))
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        edit_updates(payload, edits)
    }

    async fn execute(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let updates = update_list(&payload["updates"])?;
        let app = self.credentials.github(&session.owner).await.map_err(err)?;
        let mut added: Vec<&str> = updates
            .iter()
            .flat_map(|u| u["add_labels"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .collect();
        added.sort_unstable();
        added.dedup();
        app.ensure_labels(repo, &added).await.map_err(err)?;

        let (mut done, mut failed) = (Vec::new(), Vec::new());
        for update in &updates {
            let number = update["number"].as_u64().unwrap_or_default();
            match apply(&app, repo, number, update).await {
                Ok(()) => done.push(json!({"number": number, "did": describe(update)})),
                Err(e) => failed.push(json!({"number": number, "error": format!("{e:#}")})),
            }
        }
        if done.is_empty() {
            return Err(Error::new(format!(
                "no issues were updated: {}",
                failed[0]["error"].as_str().unwrap_or_default()
            )));
        }
        let mut message = format!(
            "Updated {}.",
            done.iter()
                .map(|u| format!(
                    "#{} ({})",
                    u["number"],
                    u["did"].as_str().unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for failure in &failed {
            message.push_str(&format!(
                " #{} failed: {}.",
                failure["number"],
                failure["error"].as_str().unwrap_or_default()
            ));
        }
        Ok(json!({"updated": done, "failed": failed, "message": message}))
    }
}

/// Makes one update. Carrying out an approved proposal again (after a restart) repeats only what's harmless to
/// repeat: a comment already on the issue isn't posted twice.
async fn apply(
    app: &crate::github::GitHubApp,
    repo: &str,
    number: u64,
    update: &Value,
) -> anyhow::Result<()> {
    if let Some(comment) = update["comment"].as_str() {
        let comments = app.issue_comments(repo, number).await?;
        if !comments.iter().any(|c| c.trim() == comment) {
            app.comment_on_issue(repo, number, comment).await?;
        }
    }
    let added: Vec<&str> = update["add_labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !added.is_empty() {
        app.add_labels(repo, number, &added).await?;
    }
    for label in update["remove_labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        app.remove_label(repo, number, label).await?;
    }
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
    fn edits_and_describes_updates() {
        let payload = json!({"repo": "a/one", "updates": [{"number": 1, "close": "duplicate", "comment": "See #2"}, {"number": 3, "reopen": true}]});
        let edited = edit_updates(
            &payload,
            &json!({"updates": [{"number": 3, "reopen": true}]}),
        )
        .unwrap();
        assert_eq!(edited["updates"], json!([{"number": 3, "reopen": true}]));
        assert!(edit_updates(&payload, &json!({"updates": []})).is_err());
        assert_eq!(
            describe(&payload["updates"][0]),
            "commented, closed as duplicate"
        );
    }
}
