//! The runtime watches the pull requests it opened and tells their sessions what changed (#107): checks failing or
//! passing again, conflicts with the base, reviews, and the pull request being merged or closed. Agents read the
//! details themselves; the runtime only tells them when to look, which they otherwise had to poll for.
//!
//! One task polls, one pull request at a time, and is the only writer of what was last seen, kept in the database so
//! a restart neither repeats nor misses a change.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::Instrument;

use crate::{AppState, db};

pub const POLL: Duration = Duration::from_secs(60);

/// A pull request as a notice names it: the repository too, since a session can push to several.
pub fn pull_ref(repo: &str, number: u64) -> String {
    format!("{repo}#{number}")
}

/// Where the user finds a pull request.
pub fn pull_url(repo: &str, number: u64) -> String {
    format!("https://github.com/{repo}/pull/{number}")
}

/// What ends a notice that a pull request needs merging by hand: the agent can't merge, so the next step is the user's.
pub const TELL_THE_USER: &str = "Tell the user, with the link, since you can't merge it yourself.";

/// What a pull request looked like when last polled. Checks and mergeability keep their last settled value, so
/// a re-run or a recomputation in progress isn't a change.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Seen {
    /// `OPEN`, `MERGED` or `CLOSED`.
    pub state: String,
    /// `MERGEABLE` or `CONFLICTING`; GitHub's `UNKNOWN` while it works it out doesn't replace either.
    pub mergeable: String,
    /// The last settled check state: `SUCCESS`, `FAILURE` or `ERROR`.
    pub checks: String,
    /// The checks that failed, when they did.
    pub failing: Vec<String>,
    /// Reviews and comments, together.
    pub feedback: u64,
}

const STATUS: &str = "query($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { \
    pullRequest(number: $number) { state mergeable reviews { totalCount } comments { totalCount } \
    reviewThreads(first: 100) { totalCount nodes { comments { totalCount } } } commits(last: 1) { nodes { commit { statusCheckRollup { state \
    contexts(first: 100) { nodes { __typename ... on CheckRun { name conclusion } \
    ... on StatusContext { context state } } } } } } } } } }";

/// What GitHub says of a pull request now, on top of what was seen before.
fn observe(previous: Option<&Seen>, pull: &Value) -> Seen {
    let previous = previous.cloned().unwrap_or_default();
    let mergeable = match pull["mergeable"].as_str() {
        Some(m @ ("MERGEABLE" | "CONFLICTING")) => m.to_owned(),
        _ => previous.mergeable.clone(),
    };
    let rollup = &pull["commits"]["nodes"][0]["commit"]["statusCheckRollup"];
    let (checks, failing) = match rollup["state"].as_str() {
        Some(state @ ("SUCCESS" | "FAILURE" | "ERROR")) => {
            let failing = rollup["contexts"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|check| match check["__typename"].as_str() {
                    Some("CheckRun") => matches!(
                        check["conclusion"].as_str(),
                        Some(
                            "FAILURE"
                                | "TIMED_OUT"
                                | "CANCELLED"
                                | "STARTUP_FAILURE"
                                | "ACTION_REQUIRED"
                        )
                    )
                    .then(|| check["name"].as_str().unwrap_or("?").to_owned()),
                    _ => matches!(check["state"].as_str(), Some("FAILURE" | "ERROR"))
                        .then(|| check["context"].as_str().unwrap_or("?").to_owned()),
                })
                .collect();
            (state.to_owned(), failing)
        }
        // Pending or expected: not settled yet.
        _ => (previous.checks.clone(), previous.failing.clone()),
    };
    let count = |field: &str| pull[field]["totalCount"].as_u64().unwrap_or(0);
    Seen {
        state: pull["state"].as_str().unwrap_or("OPEN").to_owned(),
        mergeable,
        checks,
        failing,
        // A reply in an existing thread adds a comment to it, not a thread.
        feedback: count("reviews")
            + count("comments")
            + pull["reviewThreads"]["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|thread| thread["comments"]["totalCount"].as_u64().unwrap_or(0))
                .sum::<u64>(),
    }
}

fn failed(checks: &str) -> bool {
    matches!(checks, "FAILURE" | "ERROR")
}

/// What to tell the session about pull request `number` going from `before` to `now`. The first sight of it says
/// nothing: there's no change to report.
fn changes(repo: &str, number: u64, before: Option<&Seen>, now: &Seen) -> Vec<String> {
    let Some(before) = before else {
        return Vec::new();
    };
    let pull = pull_ref(repo, number);
    let mut changes = Vec::new();
    match now.state.as_str() {
        "MERGED" if before.state != "MERGED" => changes.push(format!("{pull} was merged.")),
        "CLOSED" if before.state != "CLOSED" => {
            changes.push(format!("{pull} was closed without being merged."))
        }
        _ => {}
    }
    if now.state == "OPEN" {
        if failed(&now.checks) && !failed(&before.checks) {
            let which = if now.failing.is_empty() {
                String::new()
            } else {
                format!(" ({})", now.failing.join(", "))
            };
            changes.push(format!(
                "{pull}'s checks failed{which}; see them with `gh pr checks {number} --repo {repo}` and the logs \
                 with `gh run view <run> --repo {repo} --log-failed`."
            ));
        } else if now.checks == "SUCCESS" && failed(&before.checks) {
            changes.push(format!("{pull}'s checks pass again."));
        }
        if now.mergeable == "CONFLICTING" && before.mergeable != "CONFLICTING" {
            changes.push(format!(
                "{pull} conflicts with its base branch; merge the base into it, or rebase, and push."
            ));
        }
        if now.feedback > before.feedback {
            changes.push(format!(
                "{pull} has new reviews or comments; read them with `gh pr view {number} --repo {repo} --comments`."
            ));
        }
    }
    changes
}

pub async fn run(state: AppState) {
    let mut every = tokio::time::interval(POLL);
    loop {
        every.tick().await;
        poll(&state).await;
    }
}

async fn poll(state: &AppState) {
    let watched = match db::watched_pulls(&state.db).await {
        Ok(watched) => watched,
        Err(e) => {
            tracing::warn!(error = %e, "listing the pull requests to watch");
            return;
        }
    };
    for pull in watched {
        let span = crate::github::acting_span(&pull.session_id.to_string(), None);
        if let Err(e) = check(state, &pull).instrument(span).await {
            tracing::debug!(repo = %pull.repo, number = pull.number, error = %e, "checking a pull request");
        }
    }
}

async fn check(state: &AppState, pull: &db::WatchedPull) -> anyhow::Result<()> {
    // Watched as whoever the session acts as on the repository (ADR 29), which can read it.
    let credential = db::get_session(&state.db, pull.session_id)
        .await?
        .and_then(|session| {
            session
                .context()
                .repo_credential(&pull.repo)
                .map(str::to_owned)
        });
    crate::github::record_credential(credential.as_deref());
    let app = state
        .credentials
        .github_as(&pull.owner, credential.as_deref())
        .await?;
    let (owner, name) = pull
        .repo
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("not a repository: {}", pull.repo))?;
    let response = app
        .graphql(
            &pull.repo,
            STATUS,
            serde_json::json!({"owner": owner, "name": name, "number": pull.number}),
        )
        .await?;
    let before: Option<Seen> = pull
        .observed
        .clone()
        .and_then(|seen| serde_json::from_value(seen).ok());
    let now = observe(
        before.as_ref(),
        &response["data"]["repository"]["pullRequest"],
    );
    let changes = changes(&pull.repo, pull.number as u64, before.as_ref(), &now);
    // Told before it's saved: a restart in between tells it again rather than never.
    if !changes.is_empty() {
        let text = format!("sloppod: {}", changes.join(" "));
        state.hub.notify(pull.session_id, text).await;
    }
    if before.as_ref() != Some(&now) {
        db::save_pull_watch(
            &state.db,
            &pull.repo,
            pull.number,
            pull.session_id,
            &serde_json::to_value(&now)?,
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn seen(checks: &str, mergeable: &str, feedback: u64) -> Seen {
        Seen {
            state: "OPEN".into(),
            mergeable: mergeable.into(),
            checks: checks.into(),
            failing: Vec::new(),
            feedback,
        }
    }

    #[test]
    fn reads_what_github_says() {
        let pull = json!({
            "state": "OPEN", "mergeable": "UNKNOWN",
            "reviews": {"totalCount": 1}, "comments": {"totalCount": 2},
            "reviewThreads": {"totalCount": 1, "nodes": [{"comments": {"totalCount": 2}}]},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE", "contexts": {"nodes": [
                {"__typename": "CheckRun", "name": "check", "conclusion": "FAILURE"},
                {"__typename": "CheckRun", "name": "ui", "conclusion": "SUCCESS"},
                {"__typename": "StatusContext", "context": "ci/legacy", "state": "ERROR"},
            ]}}}}]},
        });
        let before = seen("SUCCESS", "MERGEABLE", 0);
        let now = observe(Some(&before), &pull);
        assert_eq!(now.checks, "FAILURE");
        assert_eq!(now.failing, ["check", "ci/legacy"]);
        assert_eq!(now.mergeable, "MERGEABLE", "UNKNOWN keeps what was known");
        assert_eq!(now.feedback, 5);
        // A reply in an existing thread counts, though no review, comment or thread is new.
        let mut replied = pull.clone();
        replied["reviewThreads"]["nodes"][0]["comments"]["totalCount"] = json!(3);
        assert_eq!(observe(Some(&now), &replied).feedback, 6);

        // Checks still running keep their last settled state.
        let mut running = pull.clone();
        running["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"] = json!("PENDING");
        assert_eq!(observe(Some(&now), &running).checks, "FAILURE");
    }

    #[test]
    fn tells_what_changed_and_nothing_else() {
        let ok = seen("SUCCESS", "MERGEABLE", 0);
        assert!(changes("o/r", 7, None, &ok).is_empty(), "first sight");
        assert!(changes("o/r", 7, Some(&ok), &ok).is_empty());

        let mut red = ok.clone();
        red.checks = "FAILURE".into();
        red.failing = vec!["check".into()];
        let told = changes("o/r", 7, Some(&ok), &red);
        assert_eq!(told.len(), 1);
        assert!(told[0].starts_with("o/r#7's checks failed (check)"));
        assert!(changes("o/r", 7, Some(&red), &red).is_empty(), "said once");
        assert_eq!(
            changes("o/r", 7, Some(&red), &ok),
            ["o/r#7's checks pass again."]
        );

        let conflicting = seen("SUCCESS", "CONFLICTING", 0);
        assert!(changes("o/r", 7, Some(&ok), &conflicting)[0].contains("conflicts with its base"));
        let reviewed = seen("SUCCESS", "MERGEABLE", 2);
        assert!(changes("o/r", 7, Some(&ok), &reviewed)[0].contains("new reviews or comments"));

        let mut merged = ok.clone();
        merged.state = "MERGED".into();
        assert_eq!(changes("o/r", 7, Some(&ok), &merged), ["o/r#7 was merged."]);
        let mut closed = red.clone();
        closed.state = "CLOSED".into();
        assert_eq!(
            changes("o/r", 7, Some(&ok), &closed),
            ["o/r#7 was closed without being merged."],
            "only the closing, not the checks of a closed pull request"
        );
    }
}
