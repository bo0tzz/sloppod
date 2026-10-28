//! A sub-session's report to its parent that its task is done (ADR 27). It's a proposal so that it's reviewed like
//! one, but nothing outward happens: once its review settles, the runtime delivers it.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Manifest, ProposalKind, Result, ReviewSubject, SessionContext, ToolSpec, same_repo,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::err;
use crate::credentials::Credentials;
use crate::db;

pub const REPORT: &str = "sloppod.session.report";

pub struct Report {
    db: PgPool,
    credentials: Arc<Credentials>,
}

impl Report {
    pub fn new(db: PgPool, credentials: Arc<Credentials>) -> Self {
        Self { db, credentials }
    }
}

/// What the parent is told of a report.
fn report_text(child: &str, payload: &Value) -> String {
    let field = |key: &str| payload[key].as_str().unwrap_or_default();
    let work = if payload["head_sha"].is_string() {
        format!(
            "Its work is on `{}` in {}, at {}, from `{}`.",
            field("head"),
            field("repo"),
            field("head_sha"),
            field("base")
        )
    } else {
        "It has no commits to hand over.".to_owned()
    };
    let review = &payload["review"];
    let review = match review["verdict"].as_str() {
        Some("pass") => format!(
            "Its review passed: {}",
            review["summary"].as_str().unwrap_or_default()
        ),
        Some("changes") => format!(
            "Its review still had findings after the last round:\n\n{}",
            review["findings"].as_str().unwrap_or_default()
        ),
        Some(_) => "Its review didn't finish.".to_owned(),
        None => "It wasn't reviewed.".to_owned(),
    };
    format!(
        "sloppod: your sub-session {child} reports that it's done: {}\n\n{work} {review}\n\nReview it yourself as \
         you integrate it.",
        field("summary")
    )
}

/// A report's edits: the child corrects its summary between review rounds, and the runtime adds the review's outcome
/// when it delivers the report.
fn edit_report(payload: &Value, edits: &Value) -> Result<Value> {
    let mut payload = payload.clone();
    if let Some(summary) = edits.get("summary") {
        let summary = summary
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::new("the summary can't be empty"))?;
        payload["summary"] = json!(summary);
    }
    if let Some(review) = edits.get("review") {
        payload["review"] = review.clone();
    }
    Ok(payload)
}

#[async_trait]
impl ProposalKind for Report {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("sloppod")
    }

    fn kind(&self) -> String {
        REPORT.into()
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "report_done".into(),
            description: "For a sub-session: report to your parent session that your task is done. Push your \
                          commits first. Under your review policy your branch is reviewed first, and findings come \
                          back to you like a pull request's; your parent hears once the review is over, with your \
                          summary, your branch and the review's verdict."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string", "description": "What you did, what's left, and anything your parent should check"},
                    "review": {"type": "boolean", "description": "Have another model review your branch first, whatever your policy", "default": false},
                },
                "required": ["summary"],
            }),
        }
    }

    fn outward(&self) -> bool {
        false
    }

    fn review_subject(&self, payload: &Value) -> Option<ReviewSubject> {
        let field = |key: &str| payload[key].as_str().map(str::to_owned);
        Some(ReviewSubject {
            repo: field("repo")?,
            base: field("base")?,
            head: field("head")?,
            head_sha: field("head_sha")?,
            title: field("title")?,
            body: field("summary").unwrap_or_default(),
        })
    }

    fn summary(&self, payload: &Value) -> String {
        format!(
            "report \"{}\" to your parent session",
            payload["title"].as_str().unwrap_or_default()
        )
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let summary = arguments["summary"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::new("give a summary of what you did"))?;
        let id: Uuid = session.session_id.parse().map_err(err)?;
        let row = db::get_session(&self.db, id)
            .await
            .map_err(err)?
            .ok_or_else(|| Error::new("this session no longer exists"))?;
        let parent = row
            .parent_id
            .ok_or_else(|| Error::new("only a sub-session reports to a parent"))?;
        let mut payload = json!({"parent": parent, "summary": summary, "title": row.title});
        // The branch the child was given in the repository it works in, from the parent's that it started from.
        let branch = session.grants.iter().find_map(|g| {
            let repo = g.param("repo")?;
            row.repo
                .as_deref()
                .is_some_and(|working| same_repo(working, repo))
                .then_some(())?;
            Some((repo, g.param("branch")?, g.param("from")?))
        });
        if let Some((repo, head, base)) = branch {
            let app = self.credentials.github(&session.owner).await.map_err(err)?;
            // A child spawned before its parent pushed started from the default branch, so that's its base then. A
            // branch that was never pushed has nothing to hand over; a failed comparison is retried.
            let mut base = base.to_owned();
            let mut comparison = app.compare_if_found(repo, &base, head).await.map_err(err)?;
            if comparison.is_none() {
                base = app.default_branch(repo).await.map_err(err)?;
                comparison = app.compare_if_found(repo, &base, head).await.map_err(err)?;
            }
            if let Some(comparison) = comparison
                && comparison.ahead_by > 0
            {
                payload["repo"] = json!(repo);
                payload["base"] = json!(base);
                payload["head"] = json!(head);
                payload["head_sha"] = json!(comparison.head_sha);
            }
        }
        Ok(payload)
    }

    async fn detail(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let Some(head) = payload["head_sha"].as_str() else {
            return Ok(json!({}));
        };
        let comparison = self
            .credentials
            .github(&session.owner)
            .await
            .map_err(err)?
            .compare(
                payload["repo"].as_str().unwrap_or_default(),
                payload["base"].as_str().unwrap_or_default(),
                head,
            )
            .await
            .map_err(err)?;
        Ok(json!({
            "diff": comparison.diff,
            "commits": comparison.commits.iter().map(|(sha, message)| json!({"sha": sha, "message": message})).collect::<Vec<_>>(),
        }))
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        edit_report(payload, edits)
    }

    async fn execute(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let child = format!(
            "\"{}\" ({})",
            payload["title"].as_str().unwrap_or_default(),
            session.session_id
        );
        Ok(json!({
            "message": "Your parent session has your report.",
            "notify": [{"session": payload["parent"], "text": report_text(&child, payload)}],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parent_hears_the_summary_the_branch_and_the_review() {
        let payload = json!({
            "summary": "Fixed it.", "repo": "o/r", "base": "sloppod/abcd1234",
            "head": "sloppod/abcd1234-01234567", "head_sha": "f00",
            "review": {"verdict": "changes", "findings": "- [1.1] p1 a bug"},
        });
        let text = report_text("\"Fix\" (x)", &payload);
        assert!(text.contains("reports that it's done: Fixed it."));
        assert!(
            text.contains("`sloppod/abcd1234-01234567` in o/r, at f00, from `sloppod/abcd1234`")
        );
        assert!(text.contains("still had findings after the last round:\n\n- [1.1] p1 a bug"));

        let edited = edit_report(&payload, &json!({"summary": " Fixed it properly. "})).unwrap();
        assert_eq!(edited["summary"], "Fixed it properly.");
        assert!(edit_report(&payload, &json!({"summary": " "})).is_err());

        let bare = report_text("\"Look\" (y)", &json!({"summary": "Nothing to change."}));
        assert!(bare.contains("no commits to hand over"));
        assert!(bare.contains("It wasn't reviewed."));
    }
}
