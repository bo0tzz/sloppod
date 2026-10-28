//! A session's life outside a request: placing its pod, parking and waking it, and removing it for good.

use std::collections::BTreeMap;

use serde_json::json;
use sloppod_contracts::capability;
use sloppod_contracts::{EgressSpec, Grant, PlacementSpec};

use crate::memini::MeminiAdmin;
use crate::placement::{TOKEN_DIR, WORKDIR};
use crate::pods::SESSION_LABEL;
use crate::policies::SESSION_KEY;
use crate::session::ParkError;
use uuid::Uuid;

use crate::{AppState, db};

/// Gives a session its memini key when its owner has memini and it has none, e.g. because it was just created, or
/// its key was lost (ADR 22). Keeps the namespace of an existing memory grant.
async fn ensure_memini_key(state: &AppState, session: &db::SessionRow) -> anyhow::Result<()> {
    let Some((admin, memini)) = state.credentials.memini(&session.owner).await? else {
        return Ok(());
    };
    if state
        .credentials
        .session_secret(session.id, SESSION_KEY)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let context = session.context();
    let granted = context
        .grant(capability::MEMORY)
        .and_then(|g| g.param("namespace"))
        .map(str::to_owned);
    let home = memini
        .config("home")
        .map(str::to_owned)
        .unwrap_or_else(|| format!("personal/{}", session.owner));
    let namespace = granted
        .clone()
        .unwrap_or_else(|| state.memory_namespace(session.repo.as_deref(), &home));
    let key = admin
        .create_session_key(
            &session.id.to_string(),
            memini.config("home"),
            &namespace,
            false,
        )
        .await?;
    state
        .credentials
        .put_session_secret(session.id, SESSION_KEY, &key.secret)
        .await?;
    if granted.is_none() {
        let grant = Grant::new(
            capability::MEMORY,
            json!({"namespace": namespace, "access": "rw", "key": key.name}),
        );
        db::add_grant(&state.db, session.id, &grant).await?;
    }
    Ok(())
}

/// The labels a session's egress policy selects its pod by.
pub fn pod_labels(session_id: &str) -> BTreeMap<String, String> {
    BTreeMap::from([(SESSION_LABEL.to_owned(), session_id.to_owned())])
}

/// Makes sure a session's pod and egress policy exist.
pub async fn place(state: &AppState, session: &db::SessionRow) -> anyhow::Result<()> {
    if let Err(e) = ensure_memini_key(state, session).await {
        tracing::warn!(session = %session.id, error = %e, "minting the session's memini key; no memory");
    }
    // Minting may have added a memory grant, which the placement below must see.
    let session = &db::get_session(&state.db, session.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session {} is gone", session.id))?;
    let id = session.id.to_string();
    let context = session.context();
    let labels = BTreeMap::from([
        (SESSION_LABEL.to_owned(), id.clone()),
        ("app.kubernetes.io/part-of".to_owned(), "sloppod".to_owned()),
        (
            "app.kubernetes.io/component".to_owned(),
            "session".to_owned(),
        ),
    ]);
    state
        .enforcer
        .apply(&EgressSpec {
            session_id: id.clone(),
            pod_labels: pod_labels(&id),
            intercepted_hosts: state.policies.hosts(),
            direct_hosts: if context.has(capability::PACKAGES) {
                state.config.direct_hosts.clone()
            } else {
                Vec::new()
            },
        })
        .await?;
    state
        .placement
        .ensure(&PlacementSpec {
            session_id: id,
            image: state.config.session_image.clone(),
            labels,
            env: vec![
                ("SLOPPOD_URL".into(), state.config.runtime_url()),
                ("SLOPPOD_TOKEN_FILE".into(), format!("{TOKEN_DIR}/token")),
                ("HOME".into(), format!("{WORKDIR}/home")),
                // On the volume, so scratch work outlives a restart of the pod.
                ("TMPDIR".into(), format!("{WORKDIR}/tmp")),
            ],
            cpu: state.config.session_cpu.clone(),
            memory: state.config.session_memory.clone(),
            storage_size: state.config.session_storage_size.clone(),
            storage_class: state.config.session_storage_class.clone(),
        })
        .await?;
    Ok(())
}

/// How long a deleted session can be restored before it's purged for good.
pub const RESTORABLE_FOR: chrono::Duration = chrono::Duration::days(7);

/// Purges sessions deleted longer ago than [`RESTORABLE_FOR`]: their pod, volume, egress policy and keys go.
pub async fn purge_deleted(state: &AppState) {
    let sessions = match db::claim_purges(&state.db, RESTORABLE_FOR).await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::error!(error = %e, "listing deleted sessions to purge");
            return;
        }
    };
    for session in sessions {
        // Left claimed when cleanup fails, so the next sweep tries again.
        let purged = match remove_session(state, &session).await {
            Ok(()) => db::finish_purge(&state.db, session.id)
                .await
                .map_err(Into::into),
            Err(e) => Err(e),
        };
        if let Err(e) = purged {
            tracing::warn!(session = %session.id, error = %e, "purging a deleted session");
        }
    }
}

/// Parks a session (ADR 5): its pod ends, and its volume, transcript, branch, grants and keys stay.
pub async fn park(state: &AppState, session: &db::SessionRow) -> Result<(), ParkError> {
    let id = session.id.to_string();
    state
        .hub
        .park(session.id, || async {
            state.placement.suspend(&id).await?;
            // A parked session holds no listener; waking applies its policy again. Startup releases what's left here.
            if let Err(e) = state.enforcer.release(&id, &pod_labels(&id)).await {
                tracing::warn!(session = %id, error = %e, "releasing a parked session's egress");
            }
            Ok(())
        })
        .await
}

/// How long a session has to have been quiet before its pod is replaced for a newer session image.
const ROLL_AFTER_IDLE: chrono::Duration = chrono::Duration::minutes(15);
/// The same, for a session whose supervisor doesn't say whether the agent has commands running (one from before it
/// could), so it's given longer for background work to finish.
const ROLL_AFTER_IDLE_UNKNOWN: chrono::Duration = chrono::Duration::hours(2);

/// How long a session's pod may run an older image before its agent is asked to find a quiet moment (#230).
const NUDGE_AFTER: chrono::Duration = chrono::Duration::hours(24);

/// Sessions whose agent was asked to quiet down for an image, so it's asked once per image.
#[derive(Default)]
pub struct ImageNudges(std::sync::Mutex<std::collections::HashSet<(Uuid, String)>>);

impl ImageNudges {
    /// Whether to nudge the session about `image` now; true only the first time.
    fn first(&self, session: Uuid, image: &str) -> bool {
        self.0.lock().unwrap().insert((session, image.to_owned()))
    }
}

/// Whether a session that can't be given a new pod yet should be asked to find a quiet moment: its pod has run an
/// older image for long enough, and no turn is running for the notice to interrupt.
fn due_for_nudge(
    pod_made: Option<chrono::DateTime<chrono::Utc>>,
    running: Option<&str>,
    current: &str,
    in_turn: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    !in_turn
        && running.is_some_and(|image| image != current)
        && pod_made.is_some_and(|made| now - made >= NUDGE_AFTER)
}

const NUDGE: &str = "sloppod: a newer session image is out, and this session's pod has run the old one for over a \
                     day. The pod is replaced with a new one the next time the session is quiet: idle, with nothing \
                     queued and none of your commands running. When it suits your work, wrap up or stop what you \
                     have running in the background, so that can happen. Anything still running then is ended, and \
                     you're told what was.";

/// Whether a session's pod is due to be replaced for the current session image: it runs another, and the session
/// has been idle long enough that the replacement interrupts nothing. `busy` is [`Hub::busy`](crate::session::Hub::busy).
fn due_for_image(
    session: &db::SessionRow,
    running: Option<&str>,
    current: &str,
    busy: Option<bool>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let quiet_for = match busy {
        Some(true) => return false,
        Some(false) => ROLL_AFTER_IDLE,
        None => ROLL_AFTER_IDLE_UNKNOWN,
    };
    session.phase() == sloppod_protocol::api::Phase::Idle
        && now - session.active_at >= quiet_for
        && running.is_some_and(|image| image != current)
}

/// Gives idle sessions whose pods run an older session image a new pod (#28). The pod is deleted and the Sandbox's
/// controller starts a new one from the current spec, the same path an evicted pod takes: the supervisor reports and
/// syncs on its way out, and the agent resumes its conversation in the new pod.
pub async fn roll_images(state: &AppState) {
    let sessions = match db::active_sessions(&state.db).await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::error!(error = %e, "listing sessions to roll onto the new image");
            return;
        }
    };
    let current = &state.config.session_image;
    for session in sessions {
        let pod = state.pods.session_pod(session.id);
        let running = pod.as_ref().map(|pod| pod.image.clone());
        let busy = state.hub.busy(session.id).await;
        let now = chrono::Utc::now();
        if !due_for_image(&session, running.as_deref(), current, busy, now) {
            let in_turn = state.hub.turn_running(session.id).await;
            if due_for_nudge(
                pod.and_then(|pod| pod.made),
                running.as_deref(),
                current,
                in_turn,
                now,
            ) && state.image_nudges.first(session.id, current)
            {
                state.hub.notify(session.id, NUDGE.to_owned()).await;
            }
            continue;
        }
        let id = session.id.to_string();
        let replaced = state
            .hub
            .replace_pod(session.id, || async {
                state.placement.replace_pod(&id).await?;
                Ok(())
            })
            .await;
        match replaced {
            Ok(true) => {
                tracing::info!(session = %session.id, from = ?running, to = %current, "replaced an idle session's pod for the new image");
            }
            // Something started meanwhile; the next sweep looks again.
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(session = %session.id, error = %e, "replacing a session's pod")
            }
        }
    }
}

/// Brings a parked session back: its pod starts again and the agent resumes its conversation. Other sessions are
/// left as they are, and a deleted one stays parked until it's restored.
pub async fn wake(state: &AppState, session: &db::SessionRow) -> anyhow::Result<()> {
    state
        .hub
        .wake(session.id, || async {
            // Under the session's transition lock, which deleting parks it under too.
            let current = db::get_session(&state.db, session.id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("the session is gone"))?;
            if current.deleted_at.is_some() {
                anyhow::bail!("the session is deleted; restore it from Settings first");
            }
            place(state, &current).await
        })
        .await
}

/// Stops a session and deletes its pod, volume, egress policy and keys.
pub async fn remove_session(state: &AppState, session: &db::SessionRow) -> anyhow::Result<()> {
    let id = session.id;
    state.hub.stop(id).await;
    state.placement.delete(&id.to_string()).await?;
    state.enforcer.remove(&id.to_string()).await?;
    if session.context().has(capability::MEMORY)
        && let Some((admin, _)) = state.credentials.memini(&session.owner).await?
        && let Err(e) = admin
            .delete_key(&MeminiAdmin::key_name(&id.to_string()))
            .await
    {
        tracing::warn!(session = %id, error = %e, "deleting the session's memini key");
    }
    state
        .credentials
        .delete_session_secret(id, SESSION_KEY)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_session_long_on_an_old_image_is_nudged_once() {
        let now = chrono::Utc::now();
        let day_ago = Some(now - chrono::Duration::hours(25));
        assert!(due_for_nudge(
            day_ago,
            Some("session:old"),
            "session:new",
            false,
            now
        ));
        // Not before a day, not on the current image, not mid-turn, not without a pod.
        assert!(!due_for_nudge(
            Some(now - chrono::Duration::hours(2)),
            Some("session:old"),
            "session:new",
            false,
            now
        ));
        assert!(!due_for_nudge(
            day_ago,
            Some("session:new"),
            "session:new",
            false,
            now
        ));
        assert!(!due_for_nudge(
            day_ago,
            Some("session:old"),
            "session:new",
            true,
            now
        ));
        assert!(!due_for_nudge(day_ago, None, "session:new", false, now));

        let nudges = ImageNudges::default();
        let session = Uuid::now_v7();
        assert!(nudges.first(session, "session:new"));
        assert!(!nudges.first(session, "session:new"));
        assert!(
            nudges.first(session, "session:newer"),
            "again for the next image"
        );
    }

    use chrono::{Duration, Utc};
    use sloppod_protocol::api::Phase;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn only_a_quiet_session_on_an_older_image_gets_a_new_pod() {
        let now = Utc::now();
        let session = |phase: Phase, quiet_minutes: i64| {
            let mut row = db::SessionRow::for_test(Uuid::now_v7(), "dev");
            row.phase = phase.as_str().to_owned();
            row.active_at = now - Duration::minutes(quiet_minutes);
            row
        };
        let due = |row: &db::SessionRow, image: Option<&str>, busy: Option<bool>| {
            due_for_image(row, image, "session:new", busy, now)
        };
        let idle = session(Phase::Idle, 30);
        assert!(due(&idle, Some("session:old"), Some(false)));
        // Already current, no pod, or busy (a turn, queued messages, open requests, running commands).
        assert!(!due(&idle, Some("session:new"), Some(false)));
        assert!(!due(&idle, None, Some(false)));
        assert!(!due(&idle, Some("session:old"), Some(true)));
        // Quiet for too short a time, or not idle.
        assert!(!due(
            &session(Phase::Idle, 5),
            Some("session:old"),
            Some(false)
        ));
        for phase in [
            Phase::Working,
            Phase::Waiting,
            Phase::Starting,
            Phase::Disconnected,
        ] {
            assert!(!due(&session(phase, 30), Some("session:old"), Some(false)));
        }
        // Without word on its commands, only after a long quiet.
        assert!(!due(&idle, Some("session:old"), None));
        assert!(due(&session(Phase::Idle, 150), Some("session:old"), None));
    }
}
