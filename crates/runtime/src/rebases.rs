//! Keeps auto-merge on through a push that only brings a pull request up to date (#106). A push to the head of a
//! pull request set to merge itself turns that off, since new commits haven't been reviewed (ADR 3). Once the push
//! has landed, the change the pull request makes is compared with the one that was reviewed: if it's the same, as
//! after a rebase or a merge of the base, auto-merge goes back on, pinned to the new head.

use std::time::Duration;

use sha2::{Digest, Sha256};
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
#[derive(Debug)]
pub struct Recheck {
    pub owner: String,
    pub session: Uuid,
    pub repo: String,
    pub branch: String,
    pub pulls: Vec<u64>,
    /// The commit the push sets the branch to.
    pub sha: String,
}

/// Where the authorizer hands pushes over to be rechecked once they've landed.
#[derive(Clone)]
pub struct Rechecks(mpsc::UnboundedSender<Recheck>);

impl Rechecks {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Recheck>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(tx), rx)
    }

    pub fn send(&self, recheck: Recheck) {
        let _ = self.0.send(recheck);
    }
}

pub async fn run(state: AppState, mut rechecks: mpsc::UnboundedReceiver<Recheck>) {
    while let Some(recheck) = rechecks.recv().await {
        tokio::spawn(recheck_push(state.clone(), recheck));
    }
}

#[derive(Debug, PartialEq)]
enum Outcome {
    /// The change is the one that was reviewed; auto-merge is on again.
    Kept,
    /// Auto-merge stays off, for this reason.
    Off(String),
    /// A later push moved the branch again; its own recheck says what happened.
    Superseded,
}

async fn recheck_push(state: AppState, recheck: Recheck) {
    let app = match state.credentials.github(&recheck.owner).await {
        Ok(app) => app,
        Err(e) => {
            tracing::warn!(error = %e, "no GitHub App to recheck a push with");
            return;
        }
    };
    for &pull in &recheck.pulls {
        let outcome = match recheck_pull(&state, &app, &recheck, pull).await {
            Ok(outcome) => outcome,
            Err(e) => Outcome::Off(format!("it couldn't be checked ({e:#})")),
        };
        if let Some(text) = notice(&recheck.branch, pull, &outcome) {
            state.hub.notify(recheck.session, text).await;
        }
    }
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

async fn recheck_pull(
    state: &AppState,
    app: &GitHubApp,
    recheck: &Recheck,
    number: u64,
) -> anyhow::Result<Outcome> {
    let Some(reviewed) = db::pull_fingerprint(&state.db, &recheck.repo, number).await? else {
        return Ok(Outcome::Off(
            "there's no record of what was reviewed to compare its change with".into(),
        ));
    };
    let deadline = tokio::time::Instant::now() + LANDING;
    let pull = loop {
        let pull = app.pull(&recheck.repo, number).await?;
        if pull["head"]["sha"] == recheck.sha.as_str() {
            break pull;
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(Outcome::Off("the push didn't show up on it".into()));
        }
        tokio::time::sleep(POLL).await;
    };
    if pull["state"] != "open" {
        return Ok(Outcome::Off("it's no longer open".into()));
    }
    let base = pull["base"]["ref"].as_str().unwrap_or_default();
    let comparison = app.compare(&recheck.repo, base, &recheck.branch).await?;
    if comparison.head_sha != recheck.sha {
        return Ok(Outcome::Superseded);
    }
    if fingerprint(&comparison.diff).as_deref() != Some(reviewed.as_str()) {
        return Ok(Outcome::Off(
            "the change it makes isn't the one that was reviewed".into(),
        ));
    }
    let node_id = pull["node_id"].as_str().unwrap_or_default();
    app.enable_auto_merge(&recheck.repo, node_id, &recheck.sha)
        .await?;
    Ok(Outcome::Kept)
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

    #[test]
    fn a_rebase_keeps_the_fingerprint() {
        // The same change on a base that moved: other line numbers, index and context.
        let rebased = "diff --git a/src/a.rs b/src/a.rs
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
        assert!(fingerprint(REVIEWED).is_some());
        assert_eq!(fingerprint(REVIEWED), fingerprint(rebased));
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
}
