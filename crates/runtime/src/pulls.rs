//! The runtime watches the pull requests it opened and tells their sessions what changed (#107): checks failing or
//! passing again, conflicts with the base, reviews, and the pull request being merged or closed. Agents read the
//! details themselves; the runtime only tells them when to look, which they otherwise had to poll for.
//!
//! One task polls, one pull request at a time, and is the only writer of what was last seen, kept in the database so
//! a restart neither repeats nor misses a change.

use std::collections::HashMap;
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

/// What to tell the session about pull request `number` going from `before` to `now`. At the first sight of it,
/// what's already wrong is news too (it conflicts, its checks failed, it's already merged or closed); its reviews and
/// comments so far aren't.
fn changes(repo: &str, number: u64, before: Option<&Seen>, now: &Seen) -> Vec<String> {
    let first = before.is_none();
    let before = before.cloned().unwrap_or_default();
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
        if !first && now.feedback > before.feedback {
            changes.push(format!(
                "{pull} has new reviews or comments; read them with `gh pr view {number} --repo {repo} --comments`."
            ));
        }
    }
    changes
}

pub async fn run(state: AppState) {
    let mut every = tokio::time::interval(POLL);
    let mut failures = Failures::default();
    loop {
        every.tick().await;
        poll(&state, &mut failures).await;
    }
}

/// How many polls of each pull request have failed in a row.
#[derive(Default)]
struct Failures(HashMap<(String, i64), u32>);

/// After how many failed polls in a row it's worth a warning, and then how often again: one failure is usually
/// GitHub being briefly unavailable, but a pull request that's never read again is never told about (#377).
const WARN_AFTER: u32 = 3;
const WARN_EVERY: u32 = 30;

impl Failures {
    /// Counts a failed poll, and says whether to warn about it.
    fn failed(&mut self, repo: &str, number: i64) -> bool {
        let count = self.0.entry((repo.to_owned(), number)).or_default();
        *count += 1;
        *count == WARN_AFTER
            || (*count > WARN_AFTER && (*count - WARN_AFTER).is_multiple_of(WARN_EVERY))
    }

    /// A poll that worked; says how many had failed before it, if any had been warned about.
    fn worked(&mut self, repo: &str, number: i64) -> Option<u32> {
        self.0
            .remove(&(repo.to_owned(), number))
            .filter(|count| *count >= WARN_AFTER)
    }
}

async fn poll(state: &AppState, failures: &mut Failures) {
    let watched = match db::watched_pulls(&state.db).await {
        Ok(watched) => watched,
        Err(e) => {
            tracing::warn!(error = %e, "listing the pull requests to watch");
            return;
        }
    };
    for pull in watched {
        let span = crate::github::acting_span(&pull.session_id.to_string(), None);
        match check(state, &pull).instrument(span).await {
            Ok(()) => {
                if let Some(count) = failures.worked(&pull.repo, pull.number) {
                    tracing::info!(repo = %pull.repo, number = pull.number, failed = count, "checking a pull request works again");
                }
            }
            Err(e) if failures.failed(&pull.repo, pull.number) => {
                tracing::warn!(repo = %pull.repo, number = pull.number, error = %e, "checking a pull request keeps failing; its session isn't told what changes")
            }
            Err(e) => {
                tracing::debug!(repo = %pull.repo, number = pull.number, error = %e, "checking a pull request")
            }
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
    // An answer without the pull request (GraphQL errors come with a 200) isn't an observation: read as one, it would
    // say it has no reviews or checks.
    let found = &response["data"]["repository"]["pullRequest"];
    if !found.is_object() {
        anyhow::bail!(
            "GitHub's answer has no pull request: {}",
            response["errors"]
        );
    }
    let now = observe(before.as_ref(), found);
    let changes = changes(&pull.repo, pull.number as u64, before.as_ref(), &now);
    // Items linked to it move with it (ADR 30).
    let status = crate::work::follow::pull_status(&now.state);
    if let Err(e) = crate::work::follow::follow(
        &state.db,
        &pull.owner,
        crate::work::rules::LinkKind::PullRequest,
        &pull_ref(&pull.repo, pull.number as u64),
        status,
    )
    .await
    {
        tracing::warn!(repo = %pull.repo, number = pull.number, error = ?e, "moving the items linked to a pull request");
    }
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
        assert!(
            changes("o/r", 7, None, &ok).is_empty(),
            "first sight of a good one"
        );
        assert!(
            changes("o/r", 7, None, &seen("SUCCESS", "MERGEABLE", 3)).is_empty(),
            "nor its reviews so far"
        );
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

    /// The notices a watched pull request's polls give, from GitHub's answers in turn, as `check` keeps them.
    fn told(answers: &[Value]) -> Vec<String> {
        let mut before: Option<Seen> = None;
        let mut told = Vec::new();
        for answer in answers {
            let now = observe(before.as_ref(), answer);
            told.extend(changes("o/r", 7, before.as_ref(), &now));
            before = Some(now);
        }
        told
    }

    fn mergeable(m: &str) -> Value {
        json!({"state": "OPEN", "mergeable": m})
    }

    #[test]
    fn a_conflict_is_told_once_however_it_was_seen_before() {
        let conflicts = |told: &[String]| {
            told.iter()
                .filter(|t| t.contains("conflicts with its base"))
                .count()
        };
        for answers in [
            // Mergeable, then a base change: GitHub works it out again, then conflicts.
            vec![
                "MERGEABLE",
                "UNKNOWN",
                "CONFLICTING",
                "UNKNOWN",
                "CONFLICTING",
            ],
            // Never known to be mergeable: first seen while GitHub still works it out.
            vec!["UNKNOWN", "UNKNOWN", "CONFLICTING", "CONFLICTING"],
            // Conflicting from the first look (#377).
            vec!["CONFLICTING", "UNKNOWN", "CONFLICTING"],
        ] {
            let answers: Vec<Value> = answers.into_iter().map(mergeable).collect();
            let told = told(&answers);
            assert_eq!(conflicts(&told), 1, "{answers:?}: {told:?}");
            assert_eq!(told.len(), 1, "{told:?}");
        }
        // Resolved and conflicting again is told again.
        let again = told(&["CONFLICTING", "MERGEABLE", "CONFLICTING"].map(mergeable));
        assert_eq!(conflicts(&again), 2);
    }

    #[test]
    fn failed_polls_warn_when_they_repeat() {
        let mut failures = Failures::default();
        let warned: Vec<bool> = (0..40).map(|_| failures.failed("o/r", 7)).collect();
        let at: Vec<usize> = warned
            .iter()
            .enumerate()
            .filter(|(_, w)| **w)
            .map(|(i, _)| i + 1)
            .collect();
        assert_eq!(
            at,
            [WARN_AFTER as usize, (WARN_AFTER + WARN_EVERY) as usize]
        );
        assert!(!failures.failed("o/r", 8), "counted per pull request");
        assert_eq!(failures.worked("o/r", 7), Some(40));
        assert_eq!(
            failures.worked("o/r", 8),
            None,
            "one failure wasn't warned about"
        );
        assert!(!failures.failed("o/r", 7), "counted afresh");
    }
}
