//! Keeps auto-merge on through a push that only brings a pull request up to date (#106). A push to the head of a
//! pull request set to merge itself turns that off, since new commits haven't been reviewed (ADR 3). Once the push
//! has landed, the change the pull request makes is compared with the one that was reviewed: if it's the same, as
//! after a rebase or a merge of the base, auto-merge goes back on, pinned to the new head.
//!
//! Pushes to a branch are admitted one at a time, each numbered as it is, and auto-merge only goes back on for the
//! latest, with the branch held so no push is admitted meanwhile. A push still landing never merges unreviewed.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::Instrument;
use uuid::Uuid;

use crate::github::GitHubApp;
use crate::{AppState, db};

/// How long to wait for a push the authorizer let through to show as the pull request's head.
const LANDING: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_secs(3);

/// A fingerprint of the change a diff makes, like `git patch-id`: the files it touches and the lines it adds and
/// removes, but not where they are or what's around them, so a rebase or a merge of the base doesn't change it.
/// `None` for a diff that changes binary files, whose content the diff doesn't show.
pub fn fingerprint(diff: &str) -> Option<String> {
    let mut hash = Sha256::new();
    for line in diff.lines() {
        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            return None;
        }
        if line.starts_with(' ') || line.starts_with("@@") || line.starts_with("index ") {
            continue;
        }
        hash.update(line.as_bytes());
        hash.update(b"\n");
    }
    Some(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// A push the authorizer let through to the head of pull requests whose auto-merge it turned off.
#[derive(Debug, Clone)]
pub struct Recheck {
    pub owner: String,
    /// The owner's GitHub token the session pushed with, by id, or none for the GitHub App (ADR 29); the recheck acts
    /// with the same identity.
    pub credential: Option<String>,
    pub session: Uuid,
    pub repo: String,
    pub branch: String,
    /// The pull requests, each with the fingerprint of the change that was reviewed.
    pub pulls: Vec<(u64, String)>,
    /// The commit the push sets the branch to.
    pub sha: String,
    /// The push's admission; only the latest to its branch restores auto-merge.
    pub admission: u64,
}

/// A branch's latest admitted push, and its pull requests waiting to have auto-merge restored.
#[derive(Default)]
struct Branch {
    latest: u64,
    pulls: BTreeMap<u64, String>,
}

/// A branch, shared by the guard admitting pushes to it and the rechecks restoring auto-merge on it.
type Shared = Arc<tokio::sync::Mutex<Branch>>;

/// Pushes the authorizer admitted to heads of pull requests set to merge themselves, and the rechecks they wait on.
/// Everything about one branch happens under its lock: admitting a push, and turning auto-merge back on.
pub struct Rechecks {
    db: PgPool,
    tx: mpsc::UnboundedSender<Recheck>,
    branches: Mutex<HashMap<(String, String), Shared>>,
    /// Admissions are numbered across branches, so a number is never reused.
    admissions: AtomicU64,
}

/// A push being admitted to a branch, holding the branch until it's decided.
pub struct Admission {
    branch: tokio::sync::OwnedMutexGuard<Branch>,
    pub number: u64,
}

impl Admission {
    /// The pull requests still waiting on the branch from earlier pushes.
    pub fn waiting(&self) -> Vec<u64> {
        self.branch.pulls.keys().copied().collect()
    }

    /// Has the push recheck `pulls` and those already waiting once it lands; returns them all.
    pub fn recheck(mut self, pulls: Vec<(u64, String)>) -> Vec<(u64, String)> {
        self.branch.pulls.extend(pulls);
        self.branch
            .pulls
            .iter()
            .map(|(n, f)| (*n, f.clone()))
            .collect()
    }

    /// Leaves the branch's pull requests to be merged by hand: the push couldn't be rechecked.
    pub fn forget(mut self) {
        self.branch.pulls.clear();
    }
}

impl Rechecks {
    pub fn new(db: PgPool) -> (Arc<Self>, mpsc::UnboundedReceiver<Recheck>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let rechecks = Self {
            db,
            tx,
            branches: Mutex::default(),
            admissions: AtomicU64::new(0),
        };
        (Arc::new(rechecks), rx)
    }

    fn branch(&self, repo: &str, branch: &str) -> Shared {
        self.branches
            .lock()
            .unwrap()
            .entry((repo.to_lowercase(), branch.to_owned()))
            .or_default()
            .clone()
    }

    /// Admits a push to `branch`: from now on no recheck of an earlier push restores auto-merge. The branch is held
    /// until the admission is decided, so pushes to it are admitted one at a time, in order.
    pub async fn admit(&self, repo: &str, branch: &str) -> Admission {
        let mut branch = self.branch(repo, branch).lock_owned().await;
        let number = self.admissions.fetch_add(1, Ordering::SeqCst) + 1;
        branch.latest = number;
        Admission { branch, number }
    }

    /// The fingerprints of the change each of `pulls` was reviewed with; `None` if any has none, so it can't be
    /// rechecked.
    pub async fn reviewed(
        &self,
        repo: &str,
        pulls: &[u64],
    ) -> anyhow::Result<Option<Vec<(u64, String)>>> {
        let mut reviewed = Vec::new();
        for &pull in pulls {
            match db::pull_fingerprint(&self.db, repo, pull).await? {
                Some(fingerprint) => reviewed.push((pull, fingerprint)),
                None => return Ok(None),
            }
        }
        Ok(Some(reviewed))
    }

    pub fn send(&self, recheck: Recheck) {
        let _ = self.tx.send(recheck);
    }

    async fn is_latest(&self, recheck: &Recheck) -> bool {
        self.branch(&recheck.repo, &recheck.branch)
            .lock()
            .await
            .latest
            == recheck.admission
    }
}

/// What a recheck needs from GitHub.
#[async_trait]
trait Pulls: Send + Sync {
    /// A pull request's head commit, base branch, node id and whether it's open.
    async fn pull(&self, repo: &str, number: u64) -> anyhow::Result<Value>;
    /// The diff from `base` to the commit `sha`, as of their merge base.
    async fn diff(&self, repo: &str, base: &str, sha: &str) -> anyhow::Result<String>;
    async fn enable(&self, repo: &str, node_id: &str, sha: &str) -> anyhow::Result<()>;
}

#[async_trait]
impl Pulls for GitHubApp {
    async fn pull(&self, repo: &str, number: u64) -> anyhow::Result<Value> {
        GitHubApp::pull(self, repo, number).await
    }

    async fn diff(&self, repo: &str, base: &str, sha: &str) -> anyhow::Result<String> {
        self.diff_to(repo, base, sha).await
    }

    async fn enable(&self, repo: &str, node_id: &str, sha: &str) -> anyhow::Result<()> {
        self.enable_auto_merge(repo, node_id, sha).await
    }
}

pub async fn run(state: AppState, mut rechecks: mpsc::UnboundedReceiver<Recheck>) {
    while let Some(recheck) = rechecks.recv().await {
        let state = state.clone();
        tokio::spawn(async move {
            let span = crate::github::acting_span(
                &recheck.session.to_string(),
                recheck.credential.as_deref(),
            );
            let app = match state
                .credentials
                .github_as(&recheck.owner, recheck.credential.as_deref())
                .await
            {
                Ok(app) => app,
                Err(e) => {
                    tracing::warn!(error = %e, "nothing to recheck a push with");
                    state
                        .hub
                        .notify(
                            recheck.session,
                            format!(
                                "sloppod: your push to `{}` turned auto-merge off, and it couldn't be checked to turn it \
                                 back on ({e}), so it needs merging by hand. Tell the user.",
                                recheck.branch
                            ),
                        )
                        .await;
                    return;
                }
            };
            let outcomes = recheck_push(&state.rechecks, app.as_ref(), &recheck, LANDING, POLL)
                .instrument(span)
                .await;
            for (pull, outcome) in outcomes {
                if let Some(text) = notice(&recheck.branch, pull, &outcome) {
                    state.hub.notify(recheck.session, text).await;
                }
            }
        });
    }
}

#[derive(Debug, PartialEq)]
enum Outcome {
    /// The change is the one that was reviewed; auto-merge is on again.
    Kept,
    /// Auto-merge stays off, for this reason.
    Off(String),
    /// A later push to the branch was admitted; its own recheck says what happened.
    Superseded,
}

fn notice(branch: &str, pull: u64, outcome: &Outcome) -> Option<String> {
    match outcome {
        Outcome::Kept => Some(format!(
            "sloppod: your push to `{branch}` only brought #{pull} up to date: the change it makes is the one that \
             was reviewed, so it's set to merge once its checks pass again."
        )),
        Outcome::Off(why) => Some(format!(
            "sloppod: `{branch}` is the head of #{pull}, which was set to merge once its checks pass. After your \
             push {why}, so auto-merge is off and it needs merging by hand."
        )),
        Outcome::Superseded => None,
    }
}

/// What checking one pull request found, before anything is turned back on.
enum Checked {
    /// Its change is the one reviewed; auto-merge can go back on for this node id.
    Matches(String),
    Off(String),
    Superseded,
}

/// Rechecks each pull request a push landed on, then, holding the branch, turns auto-merge back on where the change
/// is the one reviewed, if no later push has been admitted. No push can be admitted while it does.
async fn recheck_push(
    rechecks: &Rechecks,
    github: &dyn Pulls,
    recheck: &Recheck,
    landing: Duration,
    poll: Duration,
) -> Vec<(u64, Outcome)> {
    let mut checked = Vec::new();
    for (pull, reviewed) in &recheck.pulls {
        let found = check_pull(rechecks, github, recheck, *pull, reviewed, landing, poll)
            .await
            .unwrap_or_else(|e| Checked::Off(format!("it couldn't be checked ({e:#})")));
        checked.push((*pull, found));
    }
    let superseded = || {
        recheck
            .pulls
            .iter()
            .map(|(pull, _)| (*pull, Outcome::Superseded))
            .collect()
    };
    if checked
        .iter()
        .any(|(_, c)| matches!(c, Checked::Superseded))
    {
        return superseded();
    }
    let mut branch = rechecks
        .branch(&recheck.repo, &recheck.branch)
        .lock_owned()
        .await;
    if branch.latest != recheck.admission {
        return superseded();
    }
    let mut outcomes = Vec::new();
    for (pull, found) in checked {
        let outcome = match found {
            Checked::Matches(node_id) => {
                match github.enable(&recheck.repo, &node_id, &recheck.sha).await {
                    Ok(()) => Outcome::Kept,
                    Err(e) => Outcome::Off(format!("it couldn't be set to merge again ({e:#})")),
                }
            }
            Checked::Off(why) => Outcome::Off(why),
            Checked::Superseded => Outcome::Superseded,
        };
        outcomes.push((pull, outcome));
    }
    branch.pulls.clear();
    outcomes
}

async fn check_pull(
    rechecks: &Rechecks,
    github: &dyn Pulls,
    recheck: &Recheck,
    number: u64,
    reviewed: &str,
    landing: Duration,
    poll: Duration,
) -> anyhow::Result<Checked> {
    let deadline = tokio::time::Instant::now() + landing;
    let pull = loop {
        if !rechecks.is_latest(recheck).await {
            return Ok(Checked::Superseded);
        }
        let pull = github.pull(&recheck.repo, number).await?;
        if pull["head"]["sha"] == recheck.sha.as_str() {
            break pull;
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(Checked::Off("the push didn't show up on it".into()));
        }
        tokio::time::sleep(poll).await;
    };
    if pull["state"] != "open" {
        return Ok(Checked::Off("it's no longer open".into()));
    }
    let base = pull["base"]["ref"].as_str().unwrap_or_default();
    let diff = github.diff(&recheck.repo, base, &recheck.sha).await?;
    if fingerprint(&diff).as_deref() != Some(reviewed) {
        return Ok(Checked::Off(
            "the change it makes isn't the one that was reviewed".into(),
        ));
    }
    Ok(Checked::Matches(
        pull["node_id"].as_str().unwrap_or_default().to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEWED: &str = "diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,6 +10,7 @@ fn main() {
     let a = 1;
-    let b = 2;
+    let b = 3;
+    let c = 4;
     done();
 }
";

    /// The same change on a base that moved: other line numbers, index and context.
    const REBASED: &str = "diff --git a/src/a.rs b/src/a.rs
index 3333333..4444444 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -42,6 +42,7 @@ fn start() {
     let a = 10;
-    let b = 2;
+    let b = 3;
+    let c = 4;
     finish();
 }
";

    #[test]
    fn a_rebase_keeps_the_fingerprint() {
        assert!(fingerprint(REVIEWED).is_some());
        assert_eq!(fingerprint(REVIEWED), fingerprint(REBASED));
    }

    #[test]
    fn any_other_change_changes_it() {
        for changed in [
            REVIEWED.replace("let c = 4", "let c = 5"),
            REVIEWED.replace("+    let c = 4;\n", ""),
            REVIEWED.replace("src/a.rs", "src/b.rs"),
            format!(
                "{REVIEWED}diff --git a/x b/x\nnew file mode 100644\n--- /dev/null\n+++ b/x\n@@ -0,0 +1 @@\n+x\n"
            ),
        ] {
            assert_ne!(fingerprint(REVIEWED), fingerprint(&changed), "{changed}");
        }
    }

    #[test]
    fn a_binary_change_has_none() {
        let binary = format!(
            "{REVIEWED}diff --git a/logo.png b/logo.png\nindex 1..2 100644\nBinary files a/logo.png and b/logo.png differ\n"
        );
        assert_eq!(fingerprint(&binary), None);
    }

    #[test]
    fn the_agent_hears_why() {
        assert!(
            notice("s", 7, &Outcome::Kept)
                .unwrap()
                .contains("#7 up to date")
        );
        assert!(
            notice("s", 7, &Outcome::Off("it's no longer open".into()))
                .unwrap()
                .contains("After your push it's no longer open, so auto-merge is off")
        );
        assert_eq!(notice("s", 7, &Outcome::Superseded), None);
    }

    /// A fake GitHub with one pull request, #7, whose head moves to whatever `heads` says next on each lookup.
    struct Fake {
        heads: Mutex<Vec<String>>,
        diff: String,
        calls: Mutex<Vec<String>>,
        /// Held while auto-merge is being turned on, to see what can happen meanwhile.
        enabling: tokio::sync::Mutex<()>,
    }

    impl Fake {
        fn new(heads: &[&str], diff: &str) -> Self {
            Self {
                heads: Mutex::new(heads.iter().rev().map(|h| (*h).to_owned()).collect()),
                diff: diff.to_owned(),
                calls: Mutex::default(),
                enabling: tokio::sync::Mutex::default(),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Pulls for Fake {
        async fn pull(&self, _: &str, number: u64) -> anyhow::Result<Value> {
            let head = {
                let mut heads = self.heads.lock().unwrap();
                if heads.len() > 1 {
                    heads.pop().unwrap()
                } else {
                    heads[0].clone()
                }
            };
            Ok(serde_json::json!({
                "number": number, "state": "open", "node_id": format!("PR{number}"),
                "head": {"sha": head}, "base": {"ref": "main"},
            }))
        }

        async fn diff(&self, _: &str, _: &str, sha: &str) -> anyhow::Result<String> {
            self.calls.lock().unwrap().push(format!("diff {sha}"));
            Ok(self.diff.clone())
        }

        async fn enable(&self, _: &str, node_id: &str, sha: &str) -> anyhow::Result<()> {
            let _held = self.enabling.lock().await;
            self.calls
                .lock()
                .unwrap()
                .push(format!("enable {node_id} {sha}"));
            Ok(())
        }
    }

    fn rechecks() -> Arc<Rechecks> {
        let db = PgPool::connect_lazy("postgres://unused").unwrap();
        Rechecks::new(db).0
    }

    /// Admits a push of `sha` to `s-1`, the head of #7, as the guard does, and returns its recheck.
    async fn push(rechecks: &Rechecks, sha: &str) -> Recheck {
        let admission = rechecks.admit("o/r", "s-1").await;
        let number = admission.number;
        let pulls = admission.recheck(vec![(7, fingerprint(REVIEWED).unwrap())]);
        Recheck {
            owner: "dev".into(),
            credential: None,
            session: Uuid::nil(),
            repo: "o/r".into(),
            branch: "s-1".into(),
            pulls,
            sha: sha.into(),
            admission: number,
        }
    }

    const QUICK: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn a_rebase_turns_auto_merge_back_on_at_the_new_head() {
        let rechecks = rechecks();
        let recheck = push(&rechecks, "b").await;
        let github = Fake::new(&["a", "b"], REBASED);
        let outcomes = recheck_push(&rechecks, &github, &recheck, QUICK * 100, QUICK).await;
        assert_eq!(outcomes, [(7, Outcome::Kept)]);
        assert_eq!(github.calls(), ["diff b", "enable PR7 b"]);
        assert!(rechecks.admit("o/r", "s-1").await.waiting().is_empty());
    }

    #[tokio::test]
    async fn any_other_change_leaves_it_off() {
        let rechecks = rechecks();
        let recheck = push(&rechecks, "b").await;
        let github = Fake::new(&["b"], &REVIEWED.replace("let c = 4", "let c = 5"));
        let outcomes = recheck_push(&rechecks, &github, &recheck, QUICK * 100, QUICK).await;
        assert!(matches!(outcomes[..], [(7, Outcome::Off(_))]));
        assert_eq!(github.calls(), ["diff b"]);
    }

    #[tokio::test]
    async fn a_later_push_takes_over_from_an_earlier_one() {
        let rechecks = rechecks();
        let first = push(&rechecks, "b").await;
        // A second push is admitted before the first has landed; auto-merge is already off by then.
        let second = rechecks.admit("o/r", "s-1").await;
        assert_eq!(second.waiting(), [7], "the later push rechecks #7 too");
        let second = Recheck {
            admission: second.number,
            pulls: second.recheck(Vec::new()),
            sha: "c".into(),
            ..first.clone()
        };
        let github = Fake::new(&["a", "b", "c"], REBASED);
        assert_eq!(
            recheck_push(&rechecks, &github, &first, QUICK * 100, QUICK).await,
            [(7, Outcome::Superseded)]
        );
        assert_eq!(
            recheck_push(&rechecks, &github, &second, QUICK * 100, QUICK).await,
            [(7, Outcome::Kept)]
        );
        assert_eq!(github.calls(), ["diff c", "enable PR7 c"]);
    }

    #[tokio::test]
    async fn pushes_are_admitted_one_at_a_time_in_order() {
        let rechecks = rechecks();
        // A push is being admitted: auto-merge is turned off and fingerprints are looked up while it holds the branch.
        let first = rechecks.admit("o/r", "s-1").await;
        let later = {
            let rechecks = rechecks.clone();
            tokio::spawn(async move {
                let admission = rechecks.admit("o/r", "s-1").await;
                (admission.number, admission.waiting())
            })
        };
        tokio::time::sleep(QUICK * 20).await;
        assert!(
            !later.is_finished(),
            "a second push waits for the first to be decided"
        );
        let first_number = first.number;
        first.recheck(vec![(7, fingerprint(REVIEWED).unwrap())]);
        let (number, waiting) = later.await.unwrap();
        assert!(number > first_number);
        assert_eq!(waiting, [7], "and then sees what the first left waiting");
    }

    #[tokio::test]
    async fn no_push_is_admitted_while_auto_merge_goes_back_on() {
        let rechecks = rechecks();
        let first = push(&rechecks, "b").await;
        let github = Arc::new(Fake::new(&["b"], REBASED));
        // The enable mutation is slow to answer; a push comes in meanwhile.
        let enabling = github.enabling.lock().await;
        let recheck = {
            let (rechecks, github) = (rechecks.clone(), github.clone());
            tokio::spawn(async move {
                recheck_push(&rechecks, github.as_ref(), &first, QUICK * 100, QUICK).await
            })
        };
        tokio::time::sleep(QUICK * 20).await;
        let later = {
            let rechecks = rechecks.clone();
            tokio::spawn(async move { rechecks.admit("o/r", "s-1").await.number })
        };
        tokio::time::sleep(QUICK * 20).await;
        assert!(
            !later.is_finished(),
            "the push waits until auto-merge is back on"
        );
        drop(enabling);
        assert_eq!(recheck.await.unwrap(), [(7, Outcome::Kept)]);
        // Admitted only now, the push turns auto-merge off again in the guard, after the enable, never before it.
        later.await.unwrap();
    }

    #[tokio::test]
    async fn a_push_that_never_lands_leaves_it_off() {
        let rechecks = rechecks();
        let recheck = push(&rechecks, "b").await;
        let github = Fake::new(&["a"], REBASED);
        let outcomes = recheck_push(&rechecks, &github, &recheck, QUICK * 5, QUICK).await;
        assert!(matches!(outcomes[..], [(7, Outcome::Off(_))]));
        assert!(github.calls().is_empty());
    }
}
