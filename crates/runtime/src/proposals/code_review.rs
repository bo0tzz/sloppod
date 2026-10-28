//! A session's request for a review of a branch, without proposing anything (#254): work in progress, a spike, or a
//! branch it'll split up later. It's reviewed in rounds like a pull request and its findings go to the session; when
//! the review is over, nothing is carried out.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Manifest, ProposalKind, RepoGrant, Result, ReviewSubject, SessionContext, ToolSpec,
};

use super::{branch_detail, err, head_branch, target_repo};
use crate::credentials::Credentials;

pub struct CodeReview {
    credentials: Arc<Credentials>,
}

impl CodeReview {
    pub fn new(credentials: Arc<Credentials>) -> Self {
        Self { credentials }
    }
}

/// The text the session gives for what to look at, or none.
fn focus(value: &Value) -> Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::String(focus) => Ok(Some(focus.trim().to_owned()).filter(|f| !f.is_empty())),
        _ => Err(Error::new("`focus` is text")),
    }
}

/// The branch to review, with what the session asked to look at; no one decides from it.
fn subject(payload: &Value) -> Option<ReviewSubject> {
    let field = |key: &str| payload[key].as_str().map(str::to_owned);
    Some(ReviewSubject {
        repo: field("repo")?,
        base: field("base")?,
        head: field("head")?,
        head_sha: field("head_sha")?,
        title: format!("`{}`", field("head")?),
        body: field("focus").unwrap_or_default(),
        describes_change: false,
    })
}

/// Between rounds the session can say what to look at next.
fn edit_code_review(payload: &Value, edits: &Value) -> Result<Value> {
    let mut payload = payload.clone();
    if let Some(value) = edits.get("focus") {
        payload["focus"] = json!(focus(value)?);
    }
    Ok(payload)
}

#[async_trait]
impl ProposalKind for CodeReview {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("sloppod")
    }

    fn kind(&self) -> String {
        "sloppod.code_review".into()
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "request_code_review".into(),
            description: "Ask another model to review a branch without proposing anything: work in progress, a \
                          spike, or a branch you'll split up later. Push it first. It's reviewed in rounds like a \
                          pull request: findings come back to you, and you push fixes and call `request_review` \
                          for the next round, or `withdraw_proposal` once you've heard enough. Nothing is carried \
                          out when it ends, and no human is asked."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "head": {"type": "string", "description": "The branch to review: the session branch (the default) or `<session branch>-<name>`"},
                    "base": {"type": "string", "description": "The branch to compare it with; defaults to the repository's default branch"},
                    "repo": {"type": "string", "description": "Repository as owner/name; needed when the session can push to more than one"},
                    "focus": {"type": "string", "description": "What to look at in particular, e.g. a design question or a part you're unsure of"},
                },
            }),
        }
    }

    fn outward(&self) -> bool {
        false
    }

    fn is_review(&self) -> bool {
        true
    }

    fn review_subject(&self, payload: &Value) -> Option<ReviewSubject> {
        subject(payload)
    }

    fn summary(&self, payload: &Value) -> String {
        format!(
            "code review of `{}` on {}",
            payload["head"].as_str().unwrap_or_default(),
            payload["repo"].as_str().unwrap_or_default()
        )
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let RepoGrant { repo, branch } = target_repo(session, arguments["repo"].as_str())?;
        let head = head_branch(branch, arguments["head"].as_str())?;
        let app = super::github_for(&self.credentials, session, repo).await?;
        let base = match arguments["base"].as_str().filter(|b| !b.trim().is_empty()) {
            Some(base) => base.trim().to_owned(),
            None => app.default_branch(repo).await.map_err(err)?,
        };
        let comparison = app
            .compare_if_found(repo, &base, head)
            .await
            .map_err(err)?
            .ok_or_else(|| {
                Error::new(format!(
                    "`{head}` or `{base}` isn't on {repo}; push your branch first"
                ))
            })?;
        if comparison.ahead_by == 0 {
            return Err(Error::new(format!(
                "`{head}` has no commits on `{base}`; push your work first"
            )));
        }
        Ok(json!({
            "repo": repo,
            "base": base,
            "head": head,
            "head_sha": comparison.head_sha,
            "focus": focus(&arguments["focus"])?,
        }))
    }

    async fn detail(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let repo = payload["repo"].as_str().unwrap_or_default();
        let app = super::github_for(&self.credentials, session, repo).await?;
        branch_detail(&app, payload).await
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        edit_code_review(payload, edits)
    }

    async fn execute(&self, _session: &SessionContext, _payload: &Value) -> Result<Value> {
        Ok(json!({"message": "The review is over."}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Value {
        json!({"repo": "o/r", "base": "main", "head": "sloppod/abcd1234-spike", "head_sha": "f00", "focus": null})
    }

    #[test]
    fn a_code_review_is_of_a_branch_and_nobody_decides_from_it() {
        let reviewed = subject(&payload()).unwrap();
        assert_eq!(reviewed.head, "sloppod/abcd1234-spike");
        assert_eq!(reviewed.head_sha, "f00");
        assert_eq!(reviewed.body, "");
        assert!(!reviewed.describes_change);
        let mut focused = payload();
        focused["focus"] = json!("the locking");
        assert_eq!(subject(&focused).unwrap().body, "the locking");
        assert!(subject(&json!({"repo": "o/r"})).is_none());
    }

    #[test]
    fn the_focus_can_change_between_rounds_and_nothing_else() {
        let edited = edit_code_review(
            &payload(),
            &json!({"focus": " the locking ", "head": "main"}),
        )
        .unwrap();
        assert_eq!(edited["focus"], "the locking");
        assert_eq!(edited["head"], "sloppod/abcd1234-spike");
        assert_eq!(
            edit_code_review(&edited, &json!({"focus": ""})).unwrap()["focus"],
            Value::Null
        );
        assert!(edit_code_review(&payload(), &json!({"focus": 3})).is_err());
    }
}
