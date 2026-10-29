//! Filing issues in a repository the session can push to (ADR 3).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Manifest, ProposalKind, RepoGrant, Result, SessionContext, ToolSpec,
};

use super::{err, target_repo};
use crate::credentials::Credentials;
use crate::github::GitHubApp;

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

    async fn execute(
        &self,
        session: &SessionContext,
        proposal: uuid::Uuid,
        payload: &Value,
    ) -> Result<Value> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let issues = issue_list(&payload["issues"])?;
        let app = super::github_for(&self.credentials, session, repo).await?;
        file(&app, repo, proposal, &issues).await
    }
}

/// Files a proposal's issues, each marked as its own. Carrying the proposal out again (a restart before its outcome
/// was recorded) finds the ones it filed by their marks, however long ago and whatever else has the same title, and
/// files only the rest.
async fn file(
    app: &GitHubApp,
    repo: &str,
    proposal: uuid::Uuid,
    issues: &[Value],
) -> Result<Value> {
    let mut labels: Vec<&str> = issues
        .iter()
        .flat_map(|i| i["labels"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .collect();
    labels.sort_unstable();
    labels.dedup();
    app.ensure_labels(repo, &labels).await.map_err(err)?;

    let (mut filed, mut failed) = (Vec::new(), Vec::new());
    let earlier = app
        .issues_since(repo, super::made_since(proposal))
        .await
        .map_err(err)?;
    for (item, issue) in issues.iter().enumerate() {
        let marker = super::marker(proposal, item);
        if let Some(existing) = earlier.iter().find(|i| {
            i["body"]
                .as_str()
                .is_some_and(|body| body.contains(&marker))
        }) {
            filed.push(json!({"title": issue["title"], "url": existing["url"], "number": existing["number"]}));
            continue;
        }
        let mut marked = issue.clone();
        marked["body"] = json!(super::marked(
            issue["body"].as_str().unwrap_or_default(),
            &marker
        ));
        match app.create_issue(repo, &marked).await {
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

    /// A fake GitHub's issues: what's filed, each with when it was last updated, which `since` filters on.
    #[derive(Default)]
    struct Fake {
        issues: std::sync::Mutex<Vec<Value>>,
    }

    impl Fake {
        fn add(&self, title: &str, body: &str, updated_at: chrono::DateTime<chrono::Utc>) -> u64 {
            let mut issues = self.issues.lock().unwrap();
            let number = issues.len() as u64 + 1;
            issues.push(json!({
                "number": number,
                "html_url": format!("https://github.com/o/r/issues/{number}"),
                "title": title,
                "body": body,
                "updated_at": updated_at.to_rfc3339(),
            }));
            number
        }
    }

    async fn github() -> (Arc<Fake>, GitHubApp) {
        use axum::extract::{Query, State};
        use axum::routing::get;
        use axum::{Json, Router};
        type Shared = State<Arc<Fake>>;

        async fn list(
            State(fake): Shared,
            Query(query): Query<std::collections::HashMap<String, String>>,
        ) -> Json<Value> {
            let since = query
                .get("since")
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
            let issues = fake.issues.lock().unwrap();
            Json(json!(
                issues
                    .iter()
                    .filter(|i| since.is_none_or(|since| {
                        chrono::DateTime::parse_from_rfc3339(i["updated_at"].as_str().unwrap())
                            .unwrap()
                            >= since
                    }))
                    .cloned()
                    .collect::<Vec<_>>()
            ))
        }

        async fn create(State(fake): Shared, Json(issue): Json<Value>) -> Json<Value> {
            let number = fake.add(
                issue["title"].as_str().unwrap_or_default(),
                issue["body"].as_str().unwrap_or_default(),
                chrono::Utc::now(),
            );
            Json(
                json!({"number": number, "html_url": format!("https://github.com/o/r/issues/{number}")}),
            )
        }

        let fake = Arc::new(Fake::default());
        let router = Router::new()
            .route("/repos/{owner}/{name}/issues", get(list).post(create))
            .route(
                "/repos/{owner}/{name}/labels",
                get(|| async { Json(json!([])) }),
            )
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        let app = GitHubApp::for_user_with_api(&api, "token".into(), "octo".into()).unwrap();
        (fake, app)
    }

    fn proposal_at(at: chrono::DateTime<chrono::Utc>) -> uuid::Uuid {
        let secs = u64::try_from(at.timestamp()).unwrap();
        uuid::Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, secs, 0))
    }

    fn numbers(result: &Value) -> Vec<u64> {
        result["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["number"].as_u64().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn someone_else_s_issue_with_the_same_title_isn_t_taken_for_ours() {
        let (fake, app) = github().await;
        let now = chrono::Utc::now();
        fake.add("Flaky test", "Someone else's report", now);
        let issues = issue_list(&json!([{"title": "Flaky test", "body": "Ours"}])).unwrap();
        let filed = file(&app, "o/r", proposal_at(now), &issues).await.unwrap();
        assert_eq!(numbers(&filed), [2]);
        let ours = fake.issues.lock().unwrap()[1]["body"].clone();
        assert!(
            ours.as_str()
                .unwrap()
                .starts_with("Ours\n\n<!-- sloppod:proposal/")
        );
    }

    #[tokio::test]
    async fn carrying_it_out_again_days_later_files_nothing_twice() {
        let (fake, app) = github().await;
        let then = chrono::Utc::now() - chrono::Duration::days(3);
        let proposal = proposal_at(then);
        let issues = issue_list(&json!([{"title": "One"}, {"title": "Two"}])).unwrap();
        // The first time, only the first was filed before the runtime went away.
        fake.add("One", &super::super::marker(proposal, 0), then);
        let filed = file(&app, "o/r", proposal, &issues).await.unwrap();
        assert_eq!(numbers(&filed), [1, 2]);
        let again = file(&app, "o/r", proposal, &issues).await.unwrap();
        assert_eq!(numbers(&again), [1, 2]);
        assert_eq!(fake.issues.lock().unwrap().len(), 2);
    }
}
