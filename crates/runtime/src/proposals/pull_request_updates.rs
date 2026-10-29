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
            // Looked up as a pull request, so a number that is only an issue is refused here, not just on approval.
            let pull = app.pull(repo, number).await.map_err(err)?;
            current.push(json!({
                "number": number,
                "title": pull["title"].as_str().unwrap_or_default(),
                "body": pull["body"].as_str().unwrap_or_default(),
            }));
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

/// Makes one update. The pull request is looked up first, so a number that is only an issue is refused before anything
/// is written (comments, labels and edits all go through the issues API, which would take it). Then the comment and
/// labels as for any update, reopening, marking a draft ready (GitHub refuses that for a closed one), and the
/// remaining fields. Carrying out an approved proposal again (after a restart) repeats only what's harmless to repeat:
/// a pull request that's no longer a draft isn't marked again.
async fn apply(
    app: &crate::github::GitHubApp,
    repo: &str,
    number: u64,
    update: &Value,
) -> anyhow::Result<()> {
    let pull = app.pull(repo, number).await?;
    updates::comment_and_label(app, repo, number, update).await?;
    if update.get("reopen").is_some() {
        app.update_issue(repo, number, &json!({"state": "open"}))
            .await?;
    }
    if update.get("ready").is_some() && pull["draft"] == true {
        app.mark_ready(repo, pull["node_id"].as_str().unwrap_or_default())
            .await?;
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
    if !patch.is_empty() {
        app.update_issue(repo, number, &Value::Object(patch))
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::routing::{get, patch, post};
    use axum::{Json, Router};

    use super::*;
    use crate::github::GitHubApp;

    /// A fake GitHub: pull requests by number (a number that isn't one is only an issue), and every write made to it.
    #[derive(Default)]
    struct Fake {
        pulls: Mutex<HashMap<u64, Value>>,
        writes: Mutex<Vec<String>>,
    }

    type Shared = State<Arc<Fake>>;

    async fn pull(
        State(fake): Shared,
        Path((_, _, number)): Path<(String, String, u64)>,
    ) -> std::result::Result<Json<Value>, StatusCode> {
        let pulls = fake.pulls.lock().unwrap();
        pulls
            .get(&number)
            .cloned()
            .map(Json)
            .ok_or(StatusCode::NOT_FOUND)
    }

    async fn no_comments() -> Json<Value> {
        Json(json!([]))
    }

    async fn comment(
        State(fake): Shared,
        Path((_, _, number)): Path<(String, String, u64)>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        fake.writes.lock().unwrap().push(format!(
            "comment #{number}: {}",
            body["body"].as_str().unwrap_or_default()
        ));
        Json(json!({}))
    }

    async fn edit(
        State(fake): Shared,
        Path((_, _, number)): Path<(String, String, u64)>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        fake.writes
            .lock()
            .unwrap()
            .push(format!("patch #{number}: {body}"));
        if let (Some(pull), Some(state)) = (
            fake.pulls.lock().unwrap().get_mut(&number),
            body["state"].as_str(),
        ) {
            pull["state"] = json!(state);
        }
        Json(json!({}))
    }

    /// Marks the pull request ready, unless it's closed, which GitHub refuses.
    async fn graphql(State(fake): Shared, Json(body): Json<Value>) -> Json<Value> {
        let id = body["variables"]["id"].as_str().unwrap_or_default();
        let mut pulls = fake.pulls.lock().unwrap();
        let pull = pulls.values_mut().find(|p| p["node_id"] == id).unwrap();
        if pull["state"] == "closed" {
            return Json(json!({"errors": [{"message": "Pull request is closed"}]}));
        }
        pull["draft"] = json!(false);
        fake.writes.lock().unwrap().push(format!("ready {id}"));
        Json(json!({"data": {}}))
    }

    /// A client for a fake where #5 is a draft pull request and #6 a closed one, and #9 is only an issue.
    async fn github() -> (Arc<Fake>, GitHubApp) {
        let fake = Arc::new(Fake::default());
        for (number, state) in [(5, "open"), (6, "closed")] {
            fake.pulls.lock().unwrap().insert(
                number,
                json!({"number": number, "state": state, "draft": true, "node_id": format!("PR{number}"), "title": "Draft", "body": null}),
            );
        }
        let router = Router::new()
            .route("/repos/{owner}/{name}/pulls/{number}", get(pull))
            .route("/repos/{owner}/{name}/issues/{number}", patch(edit))
            .route(
                "/repos/{owner}/{name}/issues/{number}/comments",
                get(no_comments).post(comment),
            )
            .route("/graphql", post(graphql))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        let app = GitHubApp::for_user_with_api(&api, "token".into(), "octo".into()).unwrap();
        (fake, app)
    }

    #[tokio::test]
    async fn an_issue_number_is_refused_before_anything_is_written() {
        let (fake, app) = github().await;
        for update in [
            json!({"number": 9, "comment": "Hello"}),
            json!({"number": 9, "title": "New", "close": true}),
            json!({"number": 9, "ready": true, "comment": "Hello"}),
        ] {
            assert!(apply(&app, "o/r", 9, &update).await.is_err(), "{update}");
        }
        assert_eq!(*fake.writes.lock().unwrap(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_closed_draft_is_reopened_before_it_is_marked_ready() {
        let (fake, app) = github().await;
        let update = json!({"number": 6, "reopen": true, "ready": true, "title": "Ready"});
        apply(&app, "o/r", 6, &update).await.unwrap();
        assert_eq!(
            *fake.writes.lock().unwrap(),
            [
                r#"patch #6: {"state":"open"}"#,
                "ready PR6",
                r#"patch #6: {"title":"Ready"}"#
            ]
        );
        // Closed and not reopened, it can't be marked ready.
        let (_, app) = github().await;
        assert!(
            apply(&app, "o/r", 6, &json!({"number": 6, "ready": true}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn carrying_an_update_out_again_repeats_nothing() {
        let (fake, app) = github().await;
        let update = json!({"number": 5, "ready": true});
        apply(&app, "o/r", 5, &update).await.unwrap();
        apply(&app, "o/r", 5, &update).await.unwrap();
        // Marked ready once: the second time it isn't a draft any more.
        assert_eq!(*fake.writes.lock().unwrap(), ["ready PR5"]);
    }

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
