//! Room in the cluster for session pods (#454). Every session asks for the same resources at the same priority, so
//! Kubernetes never preempts one for another: a pod it can't place waits until something makes room. This says why
//! a session's pod waits, keeps image rolls from taking room while one does, and parks quiet sessions to make room
//! when lack of it is why.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sloppod_contracts::Unschedulable;
use uuid::Uuid;

use crate::{AppState, db, lifecycle};

/// How long a pod has to have been unschedulable before its session says why; a pod waits briefly while it's placed.
const SAY_AFTER: Duration = Duration::from_secs(20);
/// How long a pod has to have been unschedulable for lack of room before quiet sessions are parked to make it.
const MAKE_ROOM_AFTER: Duration = Duration::from_secs(90);
/// How long a session has to have been quiet to be parked to make room.
const QUIET_FOR: chrono::Duration = chrono::Duration::minutes(5);

/// Why a session's pod can't be placed, in words.
pub fn reason(pending: &Unschedulable) -> String {
    if pending.short_of.is_empty() {
        pending.message.clone()
    } else {
        format!(
            "waiting for room (insufficient {})",
            pending.short_of.join(" and ")
        )
    }
}

/// Whether any session pod can't be placed for lack of room: then no pod is replaced for a new image, which would
/// take room the waiting one needs, or end up waiting itself.
pub fn short_of_room(pending: &[Unschedulable]) -> bool {
    pending.iter().any(|p| !p.short_of.is_empty())
}

/// A session that could be parked to make room: up, idle, and quiet since `active_at`.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: Uuid,
    /// A review session with nothing to review: kept for a later proposal, or waiting on its author.
    pub kept_reviewer: bool,
    pub active_at: DateTime<Utc>,
    /// Nothing open for the user (a question, a permission request) and no turn, queue or command of its agent.
    pub quiet: bool,
}

/// Which sessions to park to make room, in order: one for each pod that has waited [`MAKE_ROOM_AFTER`] for lack of
/// room, kept reviewers first, then the longest quiet. A pod waiting for another reason (a taint, a volume, a quota)
/// doesn't make anything park, and neither does a session whose own pod waits. `releasing` counts parked sessions whose
/// pods are still going: the room they free is on its way, so they count against what's needed.
pub fn to_park(
    pending: &[Unschedulable],
    candidates: &[Candidate],
    releasing: usize,
    now: DateTime<Utc>,
) -> Vec<Uuid> {
    let waiting: Vec<&Unschedulable> = pending
        .iter()
        .filter(|p| !p.short_of.is_empty() && p.pending_for >= MAKE_ROOM_AFTER)
        .collect();
    if waiting.is_empty() {
        return Vec::new();
    }
    let mut parkable: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.quiet && now - c.active_at >= QUIET_FOR)
        .filter(|c| !pending.iter().any(|p| p.session_id == c.id.to_string()))
        .collect();
    parkable.sort_by_key(|c| (!c.kept_reviewer, c.active_at));
    parkable
        .into_iter()
        .take(waiting.len().saturating_sub(releasing))
        .map(|c| c.id)
        .collect()
}

/// Looks at the session pods the cluster can't place: tells their sessions' UIs why, and parks quiet sessions when
/// lack of room is why. Each park is a quiet one, so one that got busy meanwhile is left; the next check looks again.
pub async fn check(state: &AppState) {
    let pending = state.placement.unschedulable();
    let reasons: HashMap<Uuid, String> = pending
        .iter()
        .filter(|p| p.pending_for >= SAY_AFTER)
        .filter_map(|p| Some((p.session_id.parse().ok()?, reason(p))))
        .collect();
    state.hub.set_unschedulable(reasons).await;
    if !pending
        .iter()
        .any(|p| !p.short_of.is_empty() && p.pending_for >= MAKE_ROOM_AFTER)
    {
        return;
    }
    let now = Utc::now();
    let rows = match db::room_candidates(&state.db, now - QUIET_FOR).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "listing sessions to make room with");
            return;
        }
    };
    let mut candidates = Vec::new();
    for (row, kept_reviewer) in rows {
        let api = state.hub.session_api(&row).await;
        let asks = api.waiting.questions + api.waiting.permissions > 0;
        let quiet = !asks && state.hub.busy(row.id).await == Some(false);
        candidates.push(Candidate {
            id: row.id,
            kept_reviewer,
            active_at: row.active_at,
            quiet,
        });
    }
    // Parks under way: their pods may take a while (up to their termination grace) to go and free the room.
    let releasing = match db::parked_sessions(&state.db).await {
        Ok(parked) => parked
            .iter()
            .filter(|row| state.placement.has_pod(&row.id.to_string()))
            .count(),
        Err(e) => {
            tracing::warn!(error = %e, "listing parked sessions");
            return;
        }
    };
    for id in to_park(&pending, &candidates, releasing, now) {
        let Ok(Some(row)) = db::get_session(&state.db, id).await else {
            continue;
        };
        match lifecycle::park_quiet(state, &row).await {
            Ok(true) => {
                tracing::info!(session = %id, "parked a quiet session to make room for one that can't be placed")
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(session = %id, error = %e, "parking a session to make room"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(session: Uuid, message: &str, short_of: &[&str], secs: u64) -> Unschedulable {
        Unschedulable {
            session_id: session.to_string(),
            message: message.into(),
            short_of: short_of.iter().map(|s| s.to_string()).collect(),
            pending_for: Duration::from_secs(secs),
        }
    }

    fn candidate(
        minutes_quiet: i64,
        kept_reviewer: bool,
        quiet: bool,
        now: DateTime<Utc>,
    ) -> Candidate {
        Candidate {
            id: Uuid::new_v4(),
            kept_reviewer,
            active_at: now - chrono::Duration::minutes(minutes_quiet),
            quiet,
        }
    }

    #[test]
    fn a_pod_short_of_memory_makes_the_longest_quiet_session_park() {
        let now = Utc::now();
        let waiting = Uuid::new_v4();
        let memory = pending(
            waiting,
            "0/4 nodes are available: 4 Insufficient memory.",
            &["memory"],
            120,
        );
        let (recent, longest, busy) = (
            candidate(10, false, true, now),
            candidate(60, false, true, now),
            candidate(300, false, false, now),
        );
        let candidates = [recent.clone(), longest.clone(), busy];
        assert_eq!(
            to_park(std::slice::from_ref(&memory), &candidates, 0, now),
            [longest.id]
        );

        // A kept reviewer goes first, however recently it was busy.
        let reviewer = candidate(6, true, true, now);
        let candidates = [recent.clone(), longest.clone(), reviewer.clone()];
        assert_eq!(
            to_park(std::slice::from_ref(&memory), &candidates, 0, now),
            [reviewer.id]
        );

        // Not before it has waited a while, nor a session just active, nor the waiting session itself.
        let brief = pending(
            waiting,
            "0/4 nodes are available: 4 Insufficient memory.",
            &["memory"],
            30,
        );
        assert!(to_park(&[brief], std::slice::from_ref(&longest), 0, now).is_empty());
        assert!(
            to_park(
                std::slice::from_ref(&memory),
                &[candidate(2, false, true, now)],
                0,
                now
            )
            .is_empty()
        );
        let mut itself = longest.clone();
        itself.id = waiting;
        assert!(to_park(&[memory], &[itself], 0, now).is_empty());
    }

    #[test]
    fn a_park_still_releasing_its_room_counts_towards_what_a_waiting_pod_needs() {
        let now = Utc::now();
        let memory = pending(
            Uuid::new_v4(),
            "0/4 nodes are available: 4 Insufficient memory.",
            &["memory"],
            120,
        );
        let (first, second) = (
            candidate(60, false, true, now),
            candidate(30, false, true, now),
        );
        // The first check parks one.
        let parked = to_park(
            std::slice::from_ref(&memory),
            &[first.clone(), second.clone()],
            0,
            now,
        );
        assert_eq!(parked, [first.id]);
        // Its pod is still terminating at the next checks, and the same pod still waits: nothing more parks.
        for _ in 0..3 {
            assert!(
                to_park(
                    std::slice::from_ref(&memory),
                    std::slice::from_ref(&second),
                    1,
                    now
                )
                .is_empty()
            );
        }
        // Its pod gone, a pod still waiting is short of more room than that.
        assert_eq!(
            to_park(
                std::slice::from_ref(&memory),
                std::slice::from_ref(&second),
                0,
                now
            ),
            [second.id]
        );
    }

    #[test]
    fn a_pod_waiting_for_anything_but_room_parks_nothing() {
        let now = Utc::now();
        let volume = pending(
            Uuid::new_v4(),
            "0/4 nodes are available: 4 node(s) had volume node affinity conflict.",
            &[],
            600,
        );
        assert!(
            to_park(
                std::slice::from_ref(&volume),
                &[candidate(60, true, true, now)],
                0,
                now
            )
            .is_empty()
        );
        assert!(!short_of_room(std::slice::from_ref(&volume)));
        assert_eq!(reason(&volume), volume.message);
    }

    #[test]
    fn rolls_wait_while_a_pod_is_short_of_room_and_its_session_says_so() {
        let memory = pending(
            Uuid::new_v4(),
            "0/4 nodes are available: 2 Insufficient cpu, 4 Insufficient memory.",
            &["cpu", "memory"],
            5,
        );
        assert!(short_of_room(std::slice::from_ref(&memory)));
        assert_eq!(
            reason(&memory),
            "waiting for room (insufficient cpu and memory)"
        );
        assert!(!short_of_room(&[]));
    }
}
