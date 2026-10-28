//! Filing issues in a repository the session can push to (ADR 3).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Manifest, ProposalKind, RepoGrant, Result, SessionContext, ToolSpec,
};

use super::{err, target_repo};
use crate::credentials::Credentials;

pub struct Issues {
    credentials: Arc<Credentials>,
}

impl Issues {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }
}

const MAX_ISSUES: usize = 50;

/// Checks a list of issues from the agent or the reviewer, keeping only the fields GitHub gets.
fn issue_list(issues: &Value) -> Result<Vec<Value>> {
    let Some(issues) = issues.as_array().filter(|i| !i.is_empty()) else {
        return Err(Error::new("give at least one issue"));
    };
    if issues.len() > MAX_ISSUES {
        return Err(Error::new(format!(
            "propose at most {MAX_ISSUES} issues at a time"
        )));
    }
    issues
        .iter()
        .enumerate()
        .map(|(i, issue)| {
            let title = issue["title"].as_str().map(str::trim).unwrap_or_default();
            if title.is_empty() {
                return Err(Error::new(format!("issue {} has no title", i + 1)));
            }
            let labels: Vec<&str> = match &issue["labels"] {
                Value::Null => Vec::new(),
                Value::Array(labels) => labels
                    .iter()
                    .map(|l| l.as_str().map(str::trim).filter(|l| !l.is_empty()))
                    .collect::<Option<_>>()
                    .ok_or_else(|| Error::new(format!("issue {}'s labels must be names", i + 1)))?,
                _ => {
                    return Err(Error::new(format!(
                        "issue {}'s labels must be a list",
                        i + 1
                    )));
                }
            };
            Ok(json!({
                "title": title,
                "body": issue["body"].as_str().unwrap_or_default(),
                "labels": labels,
            }))
        })
        .collect()
}

/// The reviewer sends the issues to file, so dropping one leaves it out.
fn edit_issues(payload: &Value, edits: &Value) -> Result<Value> {
    let mut payload = payload.clone();
    if let Some(issues) = edits.get("issues") {
        payload["issues"] = json!(issue_list(issues)?);
    }
    Ok(payload)
}

#[async_trait]
impl ProposalKind for Issues {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("github")
    }

    fn kind(&self) -> String {
        "github.issue.create".into()
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "propose_issues".into(),
            description: "Propose filing one or more GitHub issues in a repository this session can push to. A \
                          human reviews them together, and can edit or drop any of them; once approved, sloppod \
                          files them, creating labels that don't exist yet. Returns the proposal's id; check it \
                          with get_proposal."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo": {"type": "string", "description": "Repository as owner/name; needed when the session can push to more than one"},
                    "issues": {
                        "type": "array",
                        "maxItems": MAX_ISSUES,
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "body": {"type": "string", "description": "Markdown"},
                                "labels": {"type": "array", "items": {"type": "string"}},
                            },
                            "required": ["title"],
                        },
                    },
                },
                "required": ["issues"],
            }),
        }
    }

    fn target(&self, payload: &Value) -> Option<String> {
        payload["repo"].as_str().map(str::to_owned)
    }

    fn summary(&self, payload: &Value) -> String {
        let repo = payload["repo"].as_str().unwrap_or_default();
        match payload["issues"].as_array().map(Vec::as_slice) {
            Some([issue]) => format!(
                "issue \"{}\" on {repo}",
                issue["title"].as_str().unwrap_or_default()
            ),
            Some(issues) => format!("{} issues on {repo}", issues.len()),
            None => format!("issues on {repo}"),
        }
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let RepoGrant { repo, .. } = target_repo(session, arguments["repo"].as_str())?;
        Ok(json!({"repo": repo, "issues": issue_list(&arguments["issues"])?}))
    }

    async fn detail(&self, _session: &SessionContext, _payload: &Value) -> Result<Value> {
        Ok(json!({}))
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        edit_issues(payload, edits)
    }

    async fn execute(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let issues = issue_list(&payload["issues"])?;
        let app = super::github_for(&self.credentials, session, repo).await?;
        let mut labels: Vec<&str> = issues
            .iter()
            .flat_map(|i| i["labels"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .collect();
        labels.sort_unstable();
        labels.dedup();
        app.ensure_labels(repo, &labels).await.map_err(err)?;

        let (mut filed, mut failed) = (Vec::new(), Vec::new());
        // A runtime restart can carry out an approved proposal again; don't file the same issue twice.
        let recent = app.recent_issues(repo).await.map_err(err)?;
        for issue in &issues {
            if let Some(existing) = issue["title"].as_str().and_then(|t| recent.get(t)) {
                filed.push(json!({"title": issue["title"], "url": existing["url"], "number": existing["number"]}));
                continue;
            }
            match app.create_issue(repo, issue).await {
                Ok(result) => filed.push(json!({
                    "title": issue["title"],
                    "url": result["url"],
                    "number": result["number"],
                })),
                Err(e) => failed.push(json!({"title": issue["title"], "error": e.to_string()})),
            }
        }
        if filed.is_empty() {
            return Err(Error::new(format!(
                "no issues were filed: {}",
                failed[0]["error"].as_str().unwrap_or_default()
            )));
        }
        let mut message = format!(
            "Filed {}.",
            filed
                .iter()
                .map(|i| format!("#{} {}", i["number"], i["url"].as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for failure in &failed {
            message.push_str(&format!(
                " \"{}\" failed: {}.",
                failure["title"].as_str().unwrap_or_default(),
                failure["error"].as_str().unwrap_or_default()
            ));
        }
        Ok(json!({"issues": filed, "failed": failed, "message": message}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_issue_lists() {
        let issues = issue_list(&json!([
            {"title": "  First  ", "body": "Details", "labels": ["dogfood", " ui "]},
            {"title": "Second"},
        ]))
        .unwrap();
        assert_eq!(
            issues,
            vec![
                json!({"title": "First", "body": "Details", "labels": ["dogfood", "ui"]}),
                json!({"title": "Second", "body": "", "labels": []}),
            ]
        );
        assert!(issue_list(&json!([])).is_err());
        assert!(issue_list(&json!([{"title": " "}])).is_err());
        assert!(issue_list(&json!([{"title": "a", "labels": "ui"}])).is_err());
        assert!(issue_list(&json!([{"title": "a", "labels": [""]}])).is_err());
        assert!(issue_list(&json!(vec![json!({"title": "a"}); MAX_ISSUES + 1])).is_err());
    }

    #[test]
    fn edits_issues() {
        let payload = json!({"repo": "a/one", "issues": [{"title": "One", "body": "", "labels": []}, {"title": "Two", "body": "", "labels": []}]});
        assert_eq!(edit_issues(&payload, &json!({})).unwrap(), payload);
        let edited = edit_issues(&payload, &json!({"issues": [{"title": "Two, edited"}]})).unwrap();
        assert_eq!(edited["repo"], "a/one");
        assert_eq!(
            edited["issues"],
            json!([{"title": "Two, edited", "body": "", "labels": []}])
        );
        assert!(edit_issues(&payload, &json!({"issues": []})).is_err());
    }
}
