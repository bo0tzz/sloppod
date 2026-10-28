//! Keeps auto-merge on through a push that only brings a pull request up to date (#106). A push to the head of a
//! pull request set to merge itself turns that off, since new commits haven't been reviewed (ADR 3). Once the push
//! has landed, the change the pull request makes is compared with the one that was reviewed: if it's the same, as
//! after a rebase or a merge of the base, auto-merge goes back on, pinned to the new head.
//!
//! Each branch counts the pushes admitted to it. A recheck only restores auto-merge for the latest of them, and
//! turns it off again if another push was admitted while it did, so a push still landing never merges unreviewed.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio::sync::mpsc;
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
    pub session: Uuid,
    pub repo: String,
    pub branch: String,
    /// The pull requests, each with the fingerprint of the change that was reviewed.
    pub pulls: Vec<(u64, String)>,
    /// The commit the push sets the branch to.
    pub sha: String,
    /// Which of the pushes admitted to the branch this is.
    pub generation: u64,
}

/// A branch's pull requests waiting to have auto-merge restored, and how many pushes to it were admitted.
#[derive(Default)]
struct Waiting {
    generation: u64,
    pulls: BTreeMap<u64, String>,
}

/// Pushes the authorizer admitted to heads of pull requests set to merge themselves, and the rechecks they wait on.
pub struct Rechecks {
    db: PgPool,
    tx: mpsc::UnboundedSender<Recheck>,
    branches: Mutex<HashMap<(String, String), Waiting>>,
}

fn key(repo: &str, branch: &str) -> (String, String) {
    (repo.to_lowercase(), branch.to_owned())
}

impl Rechecks {
    pub fn new(db: PgPool) -> (Arc<Self>, mpsc::UnboundedReceiver<Recheck>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let rechecks = Self {
            db,
            tx,
            branches: Mutex::default(),
        };
        (Arc::new(rechecks), rx)
    }

    /// Counts a push to `branch` as admitted, before anything about it is decided, so that a recheck of an earlier
    /// push no longer restores auto-merge. Returns the pull requests still waiting on the branch.
    pub fn admitting(&self, repo: &str, branch: &str) -> Vec<(u64, String)> {
        let mut branches = self.branches.lock().unwrap();
        match branches.get_mut(&key(repo, branch)) {
            Some(waiting) => {
                waiting.generation += 1;
                waiting.pulls.iter().map(|(n, f)| (*n, f.clone())).collect()
            }
            None => Vec::new(),
        }
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

    /// Has the push recheck `pulls` once it lands, as the latest push to its branch.
    pub fn recheck(&self, mut recheck: Recheck) {
        {
            let mut branches = self.branches.lock().unwrap();
            let waiting = branches
                .entry(key(&recheck.repo, &recheck.branch))
                .or_default();
            waiting.generation += 1;
            waiting.pulls.extend(recheck.pulls.iter().cloned());
            recheck.pulls = waiting.pulls.iter().map(|(n, f)| (*n, f.clone())).collect();
            recheck.generation = waiting.generation;
        }
        let _ = self.tx.send(recheck);
    }

    /// Forgets what was waiting on the branch: a push that couldn't be rechecked left it to be merged by hand.
    pub fn forget(&self, repo: &str, branch: &str) {
        self.branches.lock().unwrap().remove(&key(repo, branch));
    }

    fn is_latest(&self, recheck: &Recheck) -> bool {
        self.branches
            .lock()
            .unwrap()
            .get(&key(&recheck.repo, &recheck.branch))
            .is_some_and(|w| w.generation == recheck.generation)
    }

    /// Ends the wait if `recheck` is still the latest push to its branch; whether it was.
    fn conclude(&self, recheck: &Recheck) -> bool {
        let mut branches = self.branches.lock().unwrap();
        let key = key(&recheck.repo, &recheck.branch);
        if branches
            .get(&key)
            .is_some_and(|w| w.generation == recheck.generation)
        {
            branches.remove(&key);
            return true;
        }
        false
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
    async fn disable(&self, repo: &str, node_id: &str) -> anyhow::Result<()>;
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

    async fn disable(&self, repo: &str, node_id: &str) -> anyhow::Result<()> {
        self.disable_pull_auto_merge_in(repo, node_id).await
    }
}

pub async fn run(state: AppState, mut rechecks: mpsc::UnboundedReceiver<Recheck>) {
    while let Some(recheck) = rechecks.recv().await {
        let state = state.clone();
        tokio::spawn(async move {
            let app = match state.credentials.github(&recheck.owner).await {
                Ok(app) => app,
                Err(e) => {
                    tracing::warn!(error = %e, "no GitHub App to recheck a push with");
                    return;
                }
            };
            let outcomes =
                recheck_push(&state.rechecks, app.as_ref(), &recheck, LANDING, POLL).await;
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

/// Rechecks each pull request a push landed on. Auto-merge goes back on only if the push is still the latest to
/// its branch once it's on again; otherwise it's turned off again and the later push's recheck takes over.
async fn recheck_push(
    rechecks: &Rechecks,
    github: &dyn Pulls,
    recheck: &Recheck,
    landing: Duration,
    poll: Duration,
) -> Vec<(u64, Outcome)> {
    let mut outcomes = Vec::new();
    let mut enabled = Vec::new();
    for (pull, reviewed) in &recheck.pulls {
        let outcome =
            match recheck_pull(rechecks, github, recheck, *pull, reviewed, landing, poll).await {
                Ok((outcome, node_id)) => {
                    if outcome == Outcome::Kept {
                        enabled.push(node_id);
                    }
                    outcome
                }
                Err(e) => Outcome::Off(format!("it couldn't be checked ({e:#})")),
            };
        outcomes.push((*pull, outcome));
    }
    if rechecks.conclude(recheck) {
        return outcomes;
    }
    // A later push was admitted while this one was being rechecked: what it turned back on goes off again.
    for node_id in enabled {
        if let Err(e) = github.disable(&recheck.repo, &node_id).await {
            tracing::error!(repo = %recheck.repo, error = %e, "turning auto-merge off again after a later push");
        }
    }
    outcomes
        .into_iter()
        .map(|(pull, _)| (pull, Outcome::Superseded))
        .collect()
}

async fn recheck_pull(
    rechecks: &Rechecks,
    github: &dyn Pulls,
    recheck: &Recheck,
    number: u64,
    reviewed: &str,
    landing: Duration,
    poll: Duration,
) -> anyhow::Result<(Outcome, String)> {
    let deadline = tokio::time::Instant::now() + landing;
    let pull = loop {
        if !rechecks.is_latest(recheck) {
            return Ok((Outcome::Superseded, String::new()));
        }
        let pull = github.pull(&recheck.repo, number).await?;
        if pull["head"]["sha"] == recheck.sha.as_str() {
            break pull;
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok((
                Outcome::Off("the push didn't show up on it".into()),
                String::new(),
            ));
        }
        tokio::time::sleep(poll).await;
    };
    let node_id = pull["node_id"].as_str().unwrap_or_default().to_owned();
    if pull["state"] != "open" {
        return Ok((Outcome::Off("it's no longer open".into()), node_id));
    }
    let base = pull["base"]["ref"].as_str().unwrap_or_default();
    let diff = github.diff(&recheck.repo, base, &recheck.sha).await?;
    if fingerprint(&diff).as_deref() != Some(reviewed) {
        return Ok((
            Outcome::Off("the change it makes isn't the one that was reviewed".into()),
            node_id,
        ));
    }
    if !rechecks.is_latest(recheck) {
        return Ok((Outcome::Superseded, node_id));
    }
    github.enable(&recheck.repo, &node_id, &recheck.sha).await?;
    Ok((Outcome::Kept, node_id))
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
        /// Run while auto-merge is being turned on, e.g. to admit another push meanwhile.
        on_enable: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Fake {
        fn new(heads: &[&str], diff: &str) -> Self {
            Self {
                heads: Mutex::new(heads.iter().rev().map(|h| (*h).to_owned()).collect()),
                diff: diff.to_owned(),
                calls: Mutex::default(),
                on_enable: Mutex::default(),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Pulls for Fake {
        async fn pull(&self, _: &str, number: u64) -> anyhow::Result<Value> {
            let mut heads = self.heads.lock().unwrap();
            let head = if heads.len() > 1 {
                heads.pop().unwrap()
            } else {
                heads[0].clone()
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
            self.calls
                .lock()
                .unwrap()
                .push(format!("enable {node_id} {sha}"));
            if let Some(hook) = self.on_enable.lock().unwrap().take() {
                hook();
            }
            Ok(())
        }

        async fn disable(&self, _: &str, node_id: &str) -> anyhow::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("disable {node_id}"));
            Ok(())
        }
    }

    fn rechecks() -> Arc<Rechecks> {
        let db = PgPool::connect_lazy("postgres://unused").unwrap();
        Rechecks::new(db).0
    }

    /// Admits a push of `sha` to `s-1`, the head of #7, and returns its recheck.
    fn push(rechecks: &Rechecks, sha: &str) -> Recheck {
        let waiting = rechecks.admitting("o/r", "s-1");
        let mut pulls = waiting;
        if pulls.is_empty() {
            pulls.push((7, fingerprint(REVIEWED).unwrap()));
        }
        let recheck = Recheck {
            owner: "dev".into(),
            session: Uuid::nil(),
            repo: "o/r".into(),
            branch: "s-1".into(),
            pulls,
            sha: sha.into(),
            generation: 0,
        };
        rechecks.recheck(recheck.clone());
        Recheck {
            generation: rechecks.branches.lock().unwrap()[&key("o/r", "s-1")].generation,
            ..recheck
        }
    }

    const QUICK: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn a_rebase_turns_auto_merge_back_on_at_the_new_head() {
        let rechecks = rechecks();
        let recheck = push(&rechecks, "b");
        let github = Fake::new(&["a", "b"], REBASED);
        let outcomes = recheck_push(&rechecks, &github, &recheck, QUICK * 100, QUICK).await;
        assert_eq!(outcomes, [(7, Outcome::Kept)]);
        assert_eq!(github.calls(), ["diff b", "enable PR7 b"]);
        assert!(rechecks.admitting("o/r", "s-1").is_empty());
    }

    #[tokio::test]
    async fn any_other_change_leaves_it_off() {
        let rechecks = rechecks();
        let recheck = push(&rechecks, "b");
        let github = Fake::new(&["b"], &REVIEWED.replace("let c = 4", "let c = 5"));
        let outcomes = recheck_push(&rechecks, &github, &recheck, QUICK * 100, QUICK).await;
        assert!(matches!(outcomes[..], [(7, Outcome::Off(_))]));
        assert_eq!(github.calls(), ["diff b"]);
    }

    #[tokio::test]
    async fn a_later_push_takes_over_from_an_earlier_one() {
        let rechecks = rechecks();
        let first = push(&rechecks, "b");
        // A second push is admitted before the first has landed; auto-merge is already off by then.
        let second = push(&rechecks, "c");
        assert_eq!(second.pulls.len(), 1, "the later push rechecks #7 too");
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
    async fn a_push_admitted_while_auto_merge_goes_back_on_turns_it_off_again() {
        let rechecks = rechecks();
        let first = push(&rechecks, "b");
        let github = Fake::new(&["b"], REBASED);
        let during = rechecks.clone();
        *github.on_enable.lock().unwrap() = Some(Box::new(move || {
            during.admitting("o/r", "s-1");
        }));
        assert_eq!(
            recheck_push(&rechecks, &github, &first, QUICK * 100, QUICK).await,
            [(7, Outcome::Superseded)]
        );
        assert_eq!(github.calls(), ["diff b", "enable PR7 b", "disable PR7"]);
    }

    #[tokio::test]
    async fn a_push_that_never_lands_leaves_it_off() {
        let rechecks = rechecks();
        let recheck = push(&rechecks, "b");
        let github = Fake::new(&["a"], REBASED);
        let outcomes = recheck_push(&rechecks, &github, &recheck, QUICK * 5, QUICK).await;
        assert!(matches!(outcomes[..], [(7, Outcome::Off(_))]));
        assert!(github.calls().is_empty());
    }
}
