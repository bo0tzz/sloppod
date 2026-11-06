//! What sessions keep in the cluster, and what's removed once they don't need it (#455). A parked session keeps its
//! volume and its Sandbox, which owns the volume, and loses its egress policy once its pod is gone: waking applies it
//! again. A stopped session never starts again, so its policy and keys go at once, and its volume a week after it
//! stopped unless the user keeps it. A deleted one loses everything when it's purged (`lifecycle::purge_deleted`).
//! Objects whose session is purged or unknown, such as ones whose removal failed, are removed by a sweep.

use std::collections::BTreeSet;

use sloppod_protocol::api::Phase;
use uuid::Uuid;

use crate::{AppState, db, lifecycle};

/// Removes what parked and stopped sessions don't need: a parked session's egress policy once its pod is gone, and a
/// stopped one's pod, policy and keys. Each is recorded (`released_at`), so it's done once; placing the session again
/// clears it.
pub async fn release_unused(state: &AppState) {
    let sessions = match db::unreleased(&state.db).await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::warn!(error = %e, "listing sessions to release");
            return;
        }
    };
    for session in sessions {
        let id = session.id;
        let released = match session.phase() {
            Phase::Parked => release_parked(state, id).await,
            Phase::Stopped => release_stopped(state, &session).await,
            _ => continue,
        };
        match released {
            Ok(true) => {
                tracing::info!(session = %id, phase = %session.phase, "released a session's pod-side objects")
            }
            // Its pod is still going, or it woke meanwhile; the next sweep looks again.
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "releasing a session's pod-side objects")
            }
        }
    }
}

/// Removes a parked session's egress policy once its pod is gone, and records it, all under its transition lock: a
/// wake waits for it, and so can't have its newly applied policy removed. Says whether it released it.
async fn release_parked(state: &AppState, id: Uuid) -> anyhow::Result<bool> {
    let released = state
        .hub
        .while_parked(id, || async {
            // Its pod may run on for a while after it's told to stop; the fence holds until it's gone.
            if state.placement.has_pod(&id.to_string()) {
                return Ok(false);
            }
            state.hub.remove_egress(id, &*state.enforcer).await?;
            db::set_released(&state.db, id, true).await?;
            Ok(true)
        })
        .await?;
    Ok(released == Some(true))
}

/// Ends a stopped session's pod, and once it's gone removes its egress policy and keys, and records it. While the pod
/// is still going it's fenced in (denied all egress), as a parked one is. Says whether it released it.
async fn release_stopped(state: &AppState, session: &db::SessionRow) -> anyhow::Result<bool> {
    let id = session.id;
    state
        .placement
        .suspend(&id.to_string())
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    if state.placement.has_pod(&id.to_string()) {
        state.hub.release_egress(id, &*state.enforcer).await?;
        return Ok(false);
    }
    state.hub.remove_egress(id, &*state.enforcer).await?;
    lifecycle::remove_keys(state, session).await?;
    db::set_released(&state.db, id, true).await?;
    Ok(true)
}

/// Removes the volumes, and everything else, of sessions stopped longer than [`lifecycle::RESTORABLE_FOR`] whose
/// volumes the user doesn't keep. Their transcripts, proposals and branches stay: those live in the database and on
/// GitHub.
pub async fn purge_stopped(state: &AppState) {
    let sessions = match db::claim_stopped_purges(&state.db, lifecycle::RESTORABLE_FOR).await {
        Ok(sessions) => sessions,
        Err(e) => {
            tracing::warn!(error = %e, "listing stopped sessions to purge");
            return;
        }
    };
    for session in sessions {
        // A failure leaves it claimed, so the next sweep tries again at once.
        let purged = async {
            lifecycle::remove_session(state, &session).await?;
            db::finish_purge(&state.db, session.id).await?;
            // Its volume is gone: nothing of it counts towards what sessions use.
            db::clear_disk(&state.db, session.id).await?;
            anyhow::Ok(())
        }
        .await;
        match purged {
            Ok(()) => tracing::info!(session = %session.id, "removed a stopped session's volume"),
            Err(e) => {
                tracing::warn!(session = %session.id, error = %e, "purging a stopped session")
            }
        }
    }
}

/// The sessions objects are held for that have no business having any: not in the database, or purged.
pub fn leftovers(held: &BTreeSet<Uuid>, live: &[Uuid]) -> Vec<Uuid> {
    held.iter()
        .filter(|id| !live.contains(id))
        .copied()
        .collect()
}

/// Removes objects whose session is purged or unknown, through the placement backend and the enforcer (ADR 7), and
/// says what it removed.
pub async fn sweep_leftovers(state: &AppState) {
    let mut held = BTreeSet::new();
    let lists = [
        ("placement", state.placement.sessions().await),
        ("enforcer", state.enforcer.sessions().await),
    ];
    for (what, listed) in lists {
        match listed {
            Ok(sessions) => held.extend(sessions.iter().filter_map(|s| s.parse::<Uuid>().ok())),
            Err(e) => {
                // A partial list could make a live session's objects look like leftovers of nothing; it waits.
                tracing::warn!(error = %e, what, "listing the sessions objects are held for");
                return;
            }
        }
    }
    let ids: Vec<Uuid> = held.iter().copied().collect();
    let live = match db::sessions_with_objects(&state.db, &ids).await {
        Ok(live) => live,
        Err(e) => {
            tracing::warn!(error = %e, "looking up the sessions objects are held for");
            return;
        }
    };
    for id in leftovers(&held, &live) {
        let session = id.to_string();
        let removed = async {
            state
                .placement
                .delete(&session)
                .await
                .map_err(|e| anyhow::anyhow!("placement: {e}"))?;
            state
                .enforcer
                .remove(&session)
                .await
                .map_err(|e| anyhow::anyhow!("enforcer: {e}"))
        };
        match removed.await {
            Ok(()) => {
                tracing::info!(%session, "removed the objects of a purged or unknown session")
            }
            Err(e) => {
                tracing::warn!(%session, error = %e, "removing a purged or unknown session's objects")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn objects_of_unknown_or_purged_sessions_are_leftovers() {
        let (live, purged, unknown) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let held = BTreeSet::from([live, purged, unknown]);
        let mut found = leftovers(&held, &[live]);
        found.sort();
        let mut expected = vec![purged, unknown];
        expected.sort();
        assert_eq!(found, expected);
        assert!(leftovers(&held, &[live, purged, unknown]).is_empty());
    }
}
