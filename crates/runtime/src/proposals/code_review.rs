//! A session's request for a review of a branch, without proposing anything (#254): work in progress, a spike, or a
//! branch it'll split up later. It's reviewed in rounds like a pull request and its findings go to the session; when
//! the review is over, nothing is carried out.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Manifest, ProposalKind, RepoGrant, Result, ReviewSubject, SessionContext, ToolSpec,
};
use sloppod_protocol::api::Proposal;

use super::{branch_detail, err, head_branch, target_repo};
use crate::credentials::Credentials;

/// The kind of a code review's proposal, which the MCP tool answers differently from other proposals.
pub const CODE_REVIEW: &str = "sloppod.code_review";

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

/// The branch to review, with what the session asked to look at; no one decides from it. A review of the whole tree
/// has no base.
fn subject(payload: &Value) -> Option<ReviewSubject> {
    let field = |key: &str| payload[key].as_str().map(str::to_owned);
    let whole = whole(payload);
    Some(ReviewSubject {
        repo: field("repo")?,
        base: if whole { String::new() } else { field("base")? },
        head: field("head")?,
        head_sha: field("head_sha")?,
        title: format!("`{}`", field("head")?),
        body: field("focus").unwrap_or_default(),
        describes_change: false,
        whole,
    })
}

/// Whether it reviews the whole tree at `head` rather than a change (#344).
fn whole(payload: &Value) -> bool {
    payload["whole"].as_bool() == Some(true)
}

/// About as much code as a reviewer holds in its context at once: a bigger tree it can't read whole, and gets through
/// what it can in a round.
pub const READABLE_BYTES: u64 = 1_000_000;

/// Between rounds the session can say what to look at next.
fn edit_code_review(payload: &Value, edits: &Value) -> Result<Value> {
    let mut payload = payload.clone();
    if let Some(value) = edits.get("focus") {
        payload["focus"] = json!(focus(value)?);
    }
    Ok(payload)
}

/// What the session hears when its code review starts: past [`READABLE_BYTES`], that the reviewer can't read it all.
pub fn started(proposal: &Proposal) -> String {
    let mut text = format!(
        "Code review {} started; another model is reviewing the branch. Its findings come back to you. No human is \
         asked and nothing is carried out: push fixes and call request_review for the next round, or \
         withdraw_proposal once you've heard enough.",
        proposal.id
    );
    let tree = &proposal.payload["tree"];
    let (files, bytes) = (tree["files"].as_u64(), tree["bytes"].as_u64());
    if let (Some(files), Some(bytes)) = (files, bytes) {
        let truncated = tree["truncated"].as_bool() == Some(true);
        let more = if truncated {
            " or more (GitHub's listing stopped short)"
        } else {
            ""
        };
        text.push_str(&format!(
            " The tree has {files} files, {} KB{more}.",
            bytes / 1000
        ));
        if bytes > READABLE_BYTES || truncated {
            text.push_str(
                " That's more than the reviewer can read in one go, so it reads what matters most and says what it \
                 left unread; a `focus` (request_review's `update`) points it at what you care about.",
            );
        }
    }
    text
}

#[async_trait]
impl ProposalKind for CodeReview {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("sloppod")
    }

    fn kind(&self) -> String {
        CODE_REVIEW.into()
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: "request_code_review".into(),
            description: "Ask another model to review a branch without proposing anything: work in progress, a \
                          spike, or a branch you'll split up later. Push it first. It's reviewed in rounds like a \
                          pull request: findings come back to you, and you push fixes and call `request_review` \
                          for the next round, or `withdraw_proposal` once you've heard enough. Nothing is carried \
                          out when it ends, and no human is asked. With `whole`, it reviews the code at `head` as it \
                          is rather than a change, e.g. for an independent read of everything; `focus` then says \
                          what matters most."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "head": {"type": "string", "description": "The branch to review: the session branch (the default) or `<session branch>-<name>`"},
                    "base": {"type": "string", "description": "The branch to compare it with; defaults to the repository's default branch"},
                    "repo": {"type": "string", "description": "Repository as owner/name; needed when the session can push to more than one"},
                    "focus": {"type": "string", "description": "What to look at in particular, e.g. a design question or a part you're unsure of"},
                    "whole": {"type": "boolean", "description": "Review the whole tree at `head` as it is, not a change against a base; leave `base` out", "default": false},
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
        let what = if whole(payload) {
            "the whole tree at "
        } else {
            ""
        };
        format!(
            "code review of {what}`{}` on {}",
            payload["head"].as_str().unwrap_or_default(),
            payload["repo"].as_str().unwrap_or_default()
        )
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let RepoGrant { repo, branch } = target_repo(session, arguments["repo"].as_str())?;
        let head = head_branch(branch, arguments["head"].as_str())?;
        let app = super::github_for(&self.credentials, session, repo).await?;
        let focus = focus(&arguments["focus"])?;
        if whole(&arguments) {
            if arguments["base"]
                .as_str()
                .is_some_and(|b| !b.trim().is_empty())
            {
                return Err(Error::new(
                    "a review of the whole tree has no base: leave `base` out, or `whole` to review a change",
                ));
            }
            let head_sha = app.branch_sha(repo, head).await.map_err(|_| {
                Error::new(format!("`{head}` isn't on {repo}; push your branch first"))
            })?;
            let tree = app.tree_size(repo, &head_sha).await.map_err(err)?;
            return Ok(json!({
                "repo": repo,
                "base": null,
                "whole": true,
                "head": head,
                "head_sha": head_sha,
                "focus": focus,
                "tree": tree,
            }));
        }
        let base = match arguments["base"].as_str().filter(|b| !b.trim().is_empty()) {
            Some(base) => base.trim().to_owned(),
            None => app.default_branch(repo).await.map_err(err)?,
        };
        let base = super::current_base(&app, session, repo, base).await?;
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
            "focus": focus,
        }))
    }

    async fn detail(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        // The tree as it is has no diff to show.
        if whole(payload) {
            return Ok(json!({}));
        }
        let repo = payload["repo"].as_str().unwrap_or_default();
        let app = super::github_for(&self.credentials, session, repo).await?;
        branch_detail(&app, payload).await
    }

    fn edit(&self, payload: &Value, edits: &Value) -> Result<Value> {
        edit_code_review(payload, edits)
    }

    async fn execute(
        &self,
        _session: &SessionContext,
        _proposal: uuid::Uuid,
        _payload: &Value,
    ) -> Result<Value> {
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
    fn a_review_of_the_whole_tree_has_no_base() {
        let mut whole = payload();
        whole["base"] = Value::Null;
        whole["whole"] = json!(true);
        let reviewed = subject(&whole).unwrap();
        assert!(reviewed.whole);
        assert_eq!(reviewed.base, "");
        assert!(!subject(&payload()).unwrap().whole);
        // Without `whole`, a review needs its base.
        whole["whole"] = json!(false);
        assert!(subject(&whole).is_none());
    }

    #[test]
    fn the_session_hears_when_the_tree_is_more_than_a_reviewer_reads() {
        let proposal = |tree: Value| {
            let mut payload = payload();
            payload["tree"] = tree;
            serde_json::from_value::<Proposal>(json!({
                "id": uuid::Uuid::nil(), "session_id": uuid::Uuid::nil(), "kind": CODE_REVIEW, "status": "in_review",
                "payload": payload, "created_at": "2026-11-03T00:00:00Z",
            }))
            .unwrap()
        };
        let small = started(&proposal(
            json!({"files": 40, "bytes": 200_000, "truncated": false}),
        ));
        assert!(small.contains("The tree has 40 files, 200 KB."));
        assert!(!small.contains("more than the reviewer can read"));
        let big = started(&proposal(
            json!({"files": 900, "bytes": 3_000_000, "truncated": false}),
        ));
        assert!(big.contains("more than the reviewer can read"));
        let cut = started(&proposal(
            json!({"files": 100_000, "bytes": 7_000_000, "truncated": true}),
        ));
        assert!(cut.contains("or more (GitHub's listing stopped short)"));
        // A review of a change says nothing of size.
        assert!(!started(&proposal(Value::Null)).contains("The tree has"));
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
