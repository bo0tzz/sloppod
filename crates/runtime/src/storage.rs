//! What sessions keep in the cluster, and what's removed once they don't need it (#455). A parked session keeps its
//! volume and its Sandbox, which owns the volume, and loses its egress policy once its pod is gone: waking applies it
//! again. A stopped session never starts again, so its policy and keys go at once, and its volume a week after it
//! stopped unless the user keeps it. A deleted one loses everything when it's purged (`lifecycle::purge_deleted`).
//! Objects whose session is purged, such as ones whose removal failed, are removed by a sweep; ones whose session the
//! database doesn't know are only reported, as an empty or restored database would make every session unknown.
//!
//! A session's workspace also grows before it fills up (#479). Its supervisor reports how full it is; above the
//! chart's threshold the runtime asks the placement backend for a bigger claim, up to the chart's maximum. Where it
//! can't grow — the storage class doesn't allow expansion, or the maximum is reached — the session is told once, so
//! it can clean up rather than fail its next build. The last report comes as its pod stops, which a session being
//! parked sends too: one that isn't acting is told in its transcript, and its agent once it's back, rather than the
//! notice waking it (#454). A session's reports are dealt with one at a time and in the order they came in, so two
//! overlapping ones can't tell it the same thing twice.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use sloppod_contracts::{PlacementBackend, Volume};
use sloppod_protocol::api::{self, Phase};
use sqlx::PgPool;
use uuid::Uuid;

use crate::config::Config;
use crate::session::Hub;
use crate::{AppState, db, lifecycle};

/// The least one growth adds, so a small volume still gets room to work in (#479).
pub const LEAST_GROWTH: u64 = 5 << 30;

/// When and how much a session's workspace volume grows (#479), from the chart's `session.storage` values.
#[derive(Debug, Clone, Copy)]
pub struct Growth {
    /// The fraction of the volume in use that starts a growth.
    pub at: f64,
    /// How much bigger each growth makes it: a fraction of its size, and at least [`LEAST_GROWTH`].
    pub by: f64,
    /// The largest a session's workspace volume may become, in bytes.
    pub max: u64,
}

impl Growth {
    /// What the runtime was told to do, from its settings (#479).
    pub fn settings(config: &Config) -> Self {
        Self {
            at: config.session_storage_grow_at,
            by: config.session_storage_grow_by,
            max: config.session_storage_max_size,
        }
    }
}

/// How full the filesystem the pod reports has to be, while the room a session's volume was asked for hasn't arrived,
/// before the session is told to clean up: what it has to work in is all but gone (#479).
const NO_ROOM: f64 = 0.95;

/// What a disk report says to do about a session's workspace volume (#479).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// Not full enough to be worth growing, or nothing known to grow.
    Below,
    /// Filling up, but an expansion is already on its way: the next report looks again.
    Growing,
    /// An expansion is on its way and the pod's filesystem is all but out of room: the session is told to clean up,
    /// since the size the claim asks for isn't room it can use yet.
    GrowingFull,
    /// Ask the cluster for this many bytes.
    Grow(u64),
    /// Filling up, and as big as it may become.
    Full,
}

/// What a report led to: what the session is to be told, if anything, and whether it stands told that its workspace
/// can't grow any more (#479).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub notice: Option<String>,
    /// Keep it with the session, so a workspace that stays full is said once rather than at every report.
    pub full: bool,
}

/// Decides what a report means for a session's workspace volume (#479): how much of it the supervisor says is used and
/// how big the pod's filesystem is, what the cluster says the volume is now, and what the chart allows.
///
/// The volume's own size is what its use is measured against, not the size the pod's filesystem reports: a claim the
/// cluster has been asked to grow is on its way there before its filesystem catches up. That leaves the filesystem the
/// only measure of what the session can use meanwhile, so one all but out of room is told to clean up rather than left
/// waiting for room that may not arrive.
fn decide(used: u64, reported: u64, volume: Option<&Volume>, growth: Growth) -> Decision {
    let growing = volume.is_some_and(Volume::growing);
    if growing && all_but_full(used, reported) {
        return Decision::GrowingFull;
    }
    // A backend that can't say what the session's volume is, or a session whose volume is gone: nothing to grow.
    let Some(size) = volume.and_then(Volume::size) else {
        return Decision::Below;
    };
    if size == 0 || (used as f64) / (size as f64) < growth.at {
        return Decision::Below;
    }
    if growing {
        return Decision::Growing;
    }
    if size >= growth.max {
        return Decision::Full;
    }
    let step = ((size as f64) * (1.0 + growth.by)) as u64;
    Decision::Grow(step.max(size + LEAST_GROWTH).min(growth.max))
}

/// Whether the filesystem the pod reports is all but out of usable room (#479). A report that says nothing about its
/// size says nothing here either.
fn all_but_full(used: u64, size: u64) -> bool {
    size > 0 && (used as f64) / (size as f64) >= NO_ROOM
}

/// Acts on a session's disk report (#479): grows its workspace volume when the report says it's filling up, and says
/// what the session is told. `used` and `size` are as the supervisor reported them; `told` is whether it has already
/// been told it can't grow, so a workspace that stays full is said once.
pub async fn on_report(
    placement: &dyn PlacementBackend,
    session_id: &str,
    used: u64,
    size: u64,
    told: bool,
    growth: Growth,
) -> Outcome {
    let volume = match placement.volume(session_id).await {
        Ok(volume) => volume,
        // Said at the next report: an API server that won't answer now may in five minutes.
        Err(e) => {
            tracing::warn!(session = %session_id, error = %e, "reading a session's workspace volume");
            return Outcome {
                notice: None,
                full: told,
            };
        }
    };
    match decide(used, size, volume.as_ref(), growth) {
        Decision::Below => Outcome {
            notice: None,
            full: false,
        },
        Decision::Growing => Outcome {
            notice: None,
            full: told,
        },
        // An expansion is on its way, and the pod's filesystem says it's out of room meanwhile: the session is told
        // once to clean up, and told again if it fills up anew after it has room to work in.
        Decision::GrowingFull => Outcome {
            notice: (!told).then(|| pending_notice(used, size)),
            full: true,
        },
        Decision::Grow(to) => match placement.grow_volume(session_id, to).await {
            Ok(()) => {
                tracing::info!(session = %session_id, to, "grew a session's workspace volume");
                Outcome {
                    notice: Some(format!(
                        "sloppod: this session's workspace volume is being grown to {} (it was {}% full). Nothing on \
                         it was touched.",
                        size_in_words(to),
                        percent(used, size)
                    )),
                    full: false,
                }
            }
            // The storage class doesn't allow expansion, or the cluster refused it: the session is told once to
            // clean up, and the next report tries again, in case the cluster has been fixed meanwhile.
            Err(e) => {
                if told {
                    tracing::debug!(session = %session_id, error = %e, "growing a session's workspace volume");
                } else {
                    tracing::warn!(session = %session_id, error = %e, "growing a session's workspace volume");
                }
                Outcome {
                    notice: (!told).then(|| full_notice(used, size)),
                    full: true,
                }
            }
        },
        Decision::Full => Outcome {
            notice: (!told).then(|| full_notice(used, size)),
            full: true,
        },
    }
}

/// One report from a session's supervisor about how full its workspace is, and when it came in (#479): the time
/// keeps a session's reports in the order they came in, however long each of them waits on the cluster.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    pub at: Instant,
    pub used: u64,
    pub size: u64,
}

/// Acts on a disk report from a running session (#479), through the placement backend, the database, which keeps
/// whether it has been told it can't grow, and the hub, which tells it. A session's reports are dealt with one at a
/// time and in the order they came in (`Hub::on_disk_report`): two overlapping reports would both find it untold and
/// both tell it, and one that came in earlier is dropped rather than decide over a newer one.
pub async fn on_disk(
    placement: Arc<dyn PlacementBackend>,
    db: PgPool,
    hub: Arc<Hub>,
    growth: Growth,
    id: Uuid,
    report: Report,
) {
    hub.clone()
        .on_disk_report(id, report.at, || {
            act_on_report(&*placement, &db, &hub, growth, id, report)
        })
        .await;
}

/// Acts on one report (#479), as [`on_disk`] does for each of them.
async fn act_on_report(
    placement: &dyn PlacementBackend,
    db: &PgPool,
    hub: &Arc<Hub>,
    growth: Growth,
    id: Uuid,
    Report { used, size, .. }: Report,
) {
    let session = id.to_string();
    let told = match db::disk_full_told(db, id).await {
        Ok(told) => told,
        Err(e) => {
            tracing::warn!(session = %id, error = %e, "reading whether a session was told its workspace is full");
            return;
        }
    };
    let outcome = on_report(placement, &session, used, size, told, growth).await;
    let mut told_now = outcome.full;
    if let Some(notice) = outcome.notice {
        // A report comes as the pod stops too, which a session being parked or stopped has: its transcript gets the
        // notice, and its agent is told again once it's back, rather than the queue waking it (#454).
        if !hub.notify_or_record(id, "Workspace volume", notice).await {
            told_now = told;
        }
    }
    if told_now != told
        && let Err(e) = db::set_disk_full_told(db, id, told_now).await
    {
        tracing::warn!(session = %id, error = %e, "keeping that a session was told its workspace can't grow");
    }
}

/// What a session is told when its workspace is out of room while the volume it's on is still being grown (#479): the
/// size the claim asks for is on its way, but the pod's own filesystem hasn't got it yet.
fn pending_notice(used: u64, size: u64) -> String {
    format!(
        "sloppod: the workspace is {}% full and the extra room its volume was asked for hasn't arrived yet; clean \
         build output or caches.",
        percent(used, size)
    )
}

/// What a session is told when its workspace is full and can't grow any more (#479).
fn full_notice(used: u64, size: u64) -> String {
    format!(
        "sloppod: the workspace is {}% full and can't grow any more; clean build output or caches.",
        percent(used, size)
    )
}

/// How full the volume the supervisor saw is, in whole percent; 100 when it gave no size to measure against.
fn percent(used: u64, size: u64) -> u64 {
    if size == 0 {
        return 100;
    }
    (used.saturating_mul(100) / size).min(100)
}

/// A size in words, e.g. `30 GiB`, for what a session is told (#479).
fn size_in_words(bytes: u64) -> String {
    const UNITS: [(u64, &str); 5] = [
        (1 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
        (1, "B"),
    ];
    let (factor, unit) = UNITS
        .iter()
        .find(|(factor, _)| bytes >= *factor)
        .copied()
        .unwrap_or((1, "B"));
    let value = bytes as f64 / factor as f64;
    match value.fract() {
        0.0 => format!("{} {unit}", value as u64),
        _ => format!("{value:.1} {unit}"),
    }
}

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

/// Whether the session's pod is gone, as the cluster says now.
async fn pod_gone(state: &AppState, id: Uuid) -> anyhow::Result<bool> {
    state
        .placement
        .pod_gone(&id.to_string())
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))
}

/// Removes a parked session's egress policy once its pod is gone, and records it, all under its transition lock: a
/// wake waits for it, and so can't have its newly applied policy removed. Says whether it released it.
async fn release_parked(state: &AppState, id: Uuid) -> anyhow::Result<bool> {
    let released = state
        .hub
        .while_parked(id, || async {
            // Its pod may run on for a while after it's told to stop; the fence holds until it's gone.
            // Asked of the cluster, not a cache that may not have caught up (e.g. just after a restart).
            if !pod_gone(state, id).await? {
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
    if !pod_gone(state, id).await? {
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
            // Its disk use goes with it, in the same update: nothing of it counts towards what sessions use.
            db::finish_volume_removal(&state.db, session.id).await?;
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

/// What the held objects' sessions are, as the database knows them: to remove, of sessions it knows and has purged;
/// and unknown, of sessions it doesn't know at all. Unknown ones are only reported, never removed: against an empty or
/// restored database, or one of another install, every live session's objects would look unknown.
pub fn leftovers(held: &BTreeSet<Uuid>, purged: &[Uuid], known: &[Uuid]) -> (Vec<Uuid>, Vec<Uuid>) {
    let remove = held
        .iter()
        .filter(|id| purged.contains(id))
        .copied()
        .collect();
    let unknown = held
        .iter()
        .filter(|id| !known.contains(id))
        .copied()
        .collect();
    (remove, unknown)
}

/// Removes objects whose session the database knows and has purged, through the placement backend and the enforcer
/// (ADR 7), and says what it removed. Objects of sessions it doesn't know are reported, once a sweep, and left alone.
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
                tracing::warn!(error = %e, what, "listing the sessions objects are held for");
                return;
            }
        }
    }
    let ids: Vec<Uuid> = held.iter().copied().collect();
    let (known, purged) = match db::known_sessions(&state.db, &ids).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!(error = %e, "looking up the sessions objects are held for");
            return;
        }
    };
    let (remove, unknown) = leftovers(&held, &purged, &known);
    if !unknown.is_empty() {
        let ids: Vec<String> = unknown.iter().map(Uuid::to_string).collect();
        tracing::warn!(
            count = unknown.len(),
            sessions = %ids.join(", "),
            "objects of sessions the database doesn't know; left alone (see docs/deploy.md)"
        );
    }
    for id in remove {
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
            Ok(()) => tracing::info!(%session, "removed the objects of a purged session"),
            Err(e) => tracing::warn!(%session, error = %e, "removing a purged session's objects"),
        }
    }
}

/// Which of the cluster's session volumes the user sees, and whose: each volume of a session of theirs, of any kind and
/// deleted ones included, with that session; each volume whose session the database doesn't know, with none (they're
/// left alone by the sweep, and shown so they can be found); not other users' sessions' volumes.
pub fn account<'a>(
    volumes: Vec<Volume>,
    sessions: &'a [db::SessionRow],
    owner: &str,
) -> Vec<(Volume, Option<&'a db::SessionRow>)> {
    volumes
        .into_iter()
        .filter_map(|volume| {
            let id = volume
                .session_id
                .as_deref()
                .and_then(|s| s.parse::<Uuid>().ok());
            match id.and_then(|id| sessions.iter().find(|row| row.id == id)) {
                Some(row) if row.owner == owner => Some((volume, Some(row))),
                Some(_) => None,
                None => Some((volume, None)),
            }
        })
        .collect()
}

/// `GET /storage`: every volume the cluster holds for the user's sessions, and those of sessions nobody knows.
pub async fn storage(state: &AppState, owner: &str) -> anyhow::Result<api::Storage> {
    let volumes = state
        .placement
        .volumes()
        .await
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let ids: Vec<Uuid> = volumes
        .iter()
        .filter_map(|v| v.session_id.as_deref()?.parse().ok())
        .collect();
    let sessions = db::sessions_by_id(&state.db, &ids).await?;
    let mut listed = Vec::new();
    for (volume, session) in account(volumes, &sessions, owner) {
        let session = match session {
            Some(row) => Some(state.hub.session_api(row).await),
            None => None,
        };
        listed.push(api::StoredVolume {
            // Its size now: what it asks for while the cluster is on its way to it (#479).
            capacity: volume.size(),
            name: volume.name,
            session,
        });
    }
    Ok(api::Storage { volumes: listed })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use serde_json::Value;

    use super::*;

    #[test]
    fn the_user_sees_their_sessions_volumes_of_any_kind_and_unknown_ones() {
        let volume = |id: Uuid| Volume {
            name: format!("workdir-sloppod-{id}"),
            session_id: Some(id.to_string()),
            capacity: Some(20 << 30),
            requested: Some(20 << 30),
            resizing: false,
        };
        let mut parked = db::SessionRow::for_test(Uuid::new_v4(), "dev");
        parked.phase = "parked".into();
        let mut reviewer = db::SessionRow::for_test(Uuid::new_v4(), "dev");
        reviewer.kind = sloppod_protocol::api::SessionKind::Review;
        let mut deleted = db::SessionRow::for_test(Uuid::new_v4(), "dev");
        deleted.deleted_at = Some(chrono::Utc::now());
        let theirs = db::SessionRow::for_test(Uuid::new_v4(), "someone-else");
        let unknown = Uuid::new_v4();
        let rows = [
            parked.clone(),
            reviewer.clone(),
            deleted.clone(),
            theirs.clone(),
        ];
        let volumes = [parked.id, reviewer.id, deleted.id, theirs.id, unknown]
            .map(volume)
            .to_vec();
        let shown = account(volumes, &rows, "dev");
        let ids: Vec<Option<Uuid>> = shown.iter().map(|(_, row)| row.map(|r| r.id)).collect();
        assert_eq!(
            ids,
            [Some(parked.id), Some(reviewer.id), Some(deleted.id), None]
        );
        // Never measured, its claim's size still shows.
        assert!(shown[0].1.unwrap().disk_used.is_none());
        assert_eq!(shown[0].0.capacity, Some(20 << 30));
        assert_eq!(shown[3].0.name, format!("workdir-sloppod-{unknown}"));
    }

    #[test]
    fn only_a_purged_sessions_objects_are_removed_and_an_unknown_ones_are_reported() {
        let (live, purged, unknown) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let held = BTreeSet::from([live, purged, unknown]);
        let (remove, reported) = leftovers(&held, &[purged], &[live, purged]);
        assert_eq!(remove, [purged]);
        assert_eq!(reported, [unknown]);
        // Against an empty database every session is unknown: nothing is removed.
        let (remove, reported) = leftovers(&held, &[], &[]);
        assert!(remove.is_empty());
        assert_eq!(reported.len(), 3);
    }

    /// A volume the cluster is settled on: what it asks for is what it has.
    fn settled(size: u64) -> Volume {
        Volume {
            name: "workdir-sloppod-0194f0a2-0000-7000-8000-000000000000".into(),
            session_id: Some("0194f0a2-0000-7000-8000-000000000000".into()),
            capacity: Some(size),
            requested: Some(size),
            resizing: false,
        }
    }

    fn growth() -> Growth {
        Growth {
            at: 0.85,
            by: 0.5,
            max: 100 << 30,
        }
    }

    #[test]
    fn a_volume_grows_half_again_once_it_passes_the_threshold() {
        let volume = settled(20 << 30);
        // Below it, nothing happens.
        assert_eq!(
            decide(16 << 30, 20 << 30, Some(&volume), growth()),
            Decision::Below
        );
        assert_eq!(
            decide((17 << 30) - 1, 20 << 30, Some(&volume), growth()),
            Decision::Below
        );
        // At it, half again.
        assert_eq!(
            decide(17 << 30, 20 << 30, Some(&volume), growth()),
            Decision::Grow(30 << 30)
        );
    }

    #[test]
    fn a_growth_is_never_less_than_five_gib() {
        // Half again of 6 GiB is 3 GiB, so the floor is what it grows by.
        assert_eq!(
            decide(6 << 30, 6 << 30, Some(&settled(6 << 30)), growth()),
            Decision::Grow(11 << 30)
        );
    }

    #[test]
    fn a_volume_stops_at_the_maximum() {
        let growth = Growth {
            max: 60 << 30,
            ..growth()
        };
        // A step that would pass the maximum goes to it and no further.
        assert_eq!(
            decide(55 << 30, 55 << 30, Some(&settled(55 << 30)), growth),
            Decision::Grow(60 << 30)
        );
        // At it, it can't grow any more, however much of it is used.
        assert_eq!(
            decide(55 << 30, 60 << 30, Some(&settled(60 << 30)), growth),
            Decision::Full
        );
        assert_eq!(
            decide(60 << 30, 60 << 30, Some(&settled(60 << 30)), growth),
            Decision::Full
        );
    }

    #[test]
    fn nothing_grows_while_an_expansion_is_on_its_way() {
        let growth = growth();
        // The cluster is still provisioning what it was asked for: use is measured against the size the claim is on
        // its way to, and the pod's own filesystem has room to work in yet.
        let asked = Volume {
            requested: Some(30 << 30),
            ..settled(20 << 30)
        };
        assert_eq!(
            decide(18 << 30, 20 << 30, Some(&asked), growth),
            Decision::Below
        );
        // The filesystem has grown, and the resize waits on a new pod to finish: filling up, nothing more asked for.
        let waiting = Volume {
            resizing: true,
            ..settled(30 << 30)
        };
        assert_eq!(
            decide(28 << 30, 30 << 30, Some(&waiting), growth),
            Decision::Growing
        );
        // A backend that can't say what the volume is, and a session whose volume is gone.
        assert_eq!(decide(28 << 30, 30 << 30, None, growth), Decision::Below);
    }

    #[test]
    fn a_workspace_out_of_room_while_its_volume_grows_is_told_to_clean_up() {
        let growth = growth();
        // The cluster has been asked for 30 GiB and the pod's filesystem is still the 20 GiB it had, all but full:
        // the bigger size the claim asks for isn't room the session can use yet (#479).
        let asked = Volume {
            requested: Some(30 << 30),
            ..settled(20 << 30)
        };
        assert_eq!(
            decide(20 << 30, 20 << 30, Some(&asked), growth),
            Decision::GrowingFull
        );
        assert_eq!(
            decide(19 << 30, 20 << 30, Some(&asked), growth),
            Decision::GrowingFull
        );
        // Room to work in yet: left for the next report rather than told anything.
        assert_eq!(
            decide(18 << 30, 20 << 30, Some(&asked), growth),
            Decision::Below
        );
        // A filesystem already grown, with the resize waiting on a new pod: the same measure of what's left.
        let waiting = Volume {
            resizing: true,
            ..settled(30 << 30)
        };
        assert_eq!(
            decide(29 << 30, 30 << 30, Some(&waiting), growth),
            Decision::GrowingFull
        );
        assert_eq!(
            decide(28 << 30, 30 << 30, Some(&waiting), growth),
            Decision::Growing
        );
        // A report that says nothing about the filesystem's size says nothing about its room: the size the claim is
        // on its way to is all there is to go on.
        assert_eq!(
            decide(28 << 30, 0, Some(&waiting), growth),
            Decision::Growing
        );
    }

    #[test]
    fn what_a_session_is_told_reads_in_whole_percent_and_words() {
        assert_eq!(percent(18 << 30, 20 << 30), 90);
        assert_eq!(percent(55 << 30, 60 << 30), 91);
        assert_eq!(percent(1, 0), 100);
        assert_eq!(percent(30 << 30, 20 << 30), 100);
        assert_eq!(size_in_words(30 << 30), "30 GiB");
        assert_eq!(size_in_words(5 << 30), "5 GiB");
        assert_eq!(size_in_words(1 << 20), "1 MiB");
        assert_eq!(size_in_words(3 << 29), "1.5 GiB");
        assert_eq!(size_in_words(512), "512 B");
    }

    /// A placement backend with one session's volume, and a record of what it was asked for (#479).
    struct Fake {
        volume: std::sync::Mutex<Volume>,
        grown: std::sync::Mutex<Vec<u64>>,
        /// Whether it refuses to grow, as a storage class without `allowVolumeExpansion` does.
        refuses: bool,
        /// Holds every read of the volume until `released`, saying when one reaches it with `reading` (#479): what a
        /// report waiting on the cluster looks like from the outside.
        holds_reads: std::sync::atomic::AtomicBool,
        reading: tokio::sync::Notify,
        released: tokio::sync::Notify,
    }

    impl Fake {
        fn new(volume: Volume, refuses: bool) -> Self {
            Self {
                volume: std::sync::Mutex::new(volume),
                grown: std::sync::Mutex::default(),
                refuses,
                holds_reads: std::sync::atomic::AtomicBool::default(),
                reading: tokio::sync::Notify::new(),
                released: tokio::sync::Notify::new(),
            }
        }

        fn grown(&self) -> Vec<u64> {
            self.grown.lock().unwrap().clone()
        }

        /// Waits at every read of the volume until the test lets it go.
        async fn held(&self) {
            if self.holds_reads.load(Ordering::SeqCst) {
                self.reading.notify_one();
                self.released.notified().await;
            }
        }
    }

    #[async_trait::async_trait]
    impl PlacementBackend for Fake {
        fn manifest(&self) -> sloppod_contracts::Manifest {
            sloppod_contracts::Manifest::builtin("fake")
        }

        async fn ensure(
            &self,
            _spec: &sloppod_contracts::PlacementSpec,
        ) -> sloppod_contracts::Result<()> {
            unreachable!("nothing places a pod here")
        }

        async fn suspend(&self, _session_id: &str) -> sloppod_contracts::Result<()> {
            unreachable!("nothing suspends a pod here")
        }

        async fn replace_pod(&self, _session_id: &str) -> sloppod_contracts::Result<()> {
            unreachable!("nothing replaces a pod here")
        }

        async fn delete(&self, _session_id: &str) -> sloppod_contracts::Result<()> {
            unreachable!("nothing deletes a pod here")
        }

        async fn volume(&self, _session_id: &str) -> sloppod_contracts::Result<Option<Volume>> {
            self.held().await;
            Ok(Some(self.volume.lock().unwrap().clone()))
        }

        async fn grow_volume(&self, _session_id: &str, size: u64) -> sloppod_contracts::Result<()> {
            if self.refuses {
                return Err(sloppod_contracts::Error::new(
                    "persistentvolumeclaims is forbidden: the storage class doesn't allow expansion",
                ));
            }
            self.grown.lock().unwrap().push(size);
            // As the cluster does: the claim asks for it from here on, and its filesystem catches up later.
            self.volume.lock().unwrap().requested = Some(size);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_report_above_the_threshold_grows_the_claim_and_says_so() {
        let fake = Fake::new(settled(20 << 30), false);
        let outcome = on_report(&fake, "s", 18 << 30, 20 << 30, false, growth()).await;
        assert_eq!(fake.grown(), [30 << 30]);
        assert_eq!(
            outcome.notice.as_deref(),
            Some(
                "sloppod: this session's workspace volume is being grown to 30 GiB (it was 90% full). Nothing on it \
                 was touched."
            )
        );
        assert!(!outcome.full);

        // The report that follows, while the cluster is still expanding it, grows nothing more.
        let next = on_report(&fake, "s", 18 << 30, 20 << 30, outcome.full, growth()).await;
        assert_eq!(fake.grown(), [30 << 30]);
        assert_eq!(next.notice, None);
    }

    #[tokio::test]
    async fn a_workspace_that_can_t_grow_is_told_once_and_not_at_every_report() {
        let maxed = Growth {
            max: 60 << 30,
            ..growth()
        };
        let fake = Fake::new(settled(60 << 30), false);
        let first = on_report(&fake, "s", 55 << 30, 60 << 30, false, maxed).await;
        assert!(first.full);
        assert_eq!(
            first.notice.as_deref(),
            Some(
                "sloppod: the workspace is 91% full and can't grow any more; clean build output or caches."
            )
        );
        assert!(fake.grown().is_empty());

        // Five minutes later it's just as full: nothing more is said.
        let next = on_report(&fake, "s", 56 << 30, 60 << 30, first.full, maxed).await;
        assert_eq!(next.notice, None);
        assert!(next.full);

        // Cleaned up, it's below the threshold again, and a workspace that fills up anew is worth saying again.
        let quiet = on_report(&fake, "s", 10 << 30, 60 << 30, next.full, maxed).await;
        assert_eq!(quiet.notice, None);
        assert!(!quiet.full);
        let again = on_report(&fake, "s", 55 << 30, 60 << 30, quiet.full, maxed).await;
        assert!(again.notice.is_some());
    }

    #[tokio::test]
    async fn a_storage_class_that_won_t_expand_has_the_session_told_once() {
        let fake = Fake::new(settled(20 << 30), true);
        let first = on_report(&fake, "s", 18 << 30, 20 << 30, false, growth()).await;
        assert!(first.full);
        assert_eq!(
            first.notice.as_deref(),
            Some(
                "sloppod: the workspace is 90% full and can't grow any more; clean build output or caches."
            )
        );

        // It's tried again at the next report — the cluster may allow it by then — but nothing more is said.
        let next = on_report(&fake, "s", 18 << 30, 20 << 30, first.full, growth()).await;
        assert_eq!(next.notice, None);
        assert!(next.full);
    }

    #[tokio::test]
    async fn a_workspace_out_of_room_while_its_volume_grows_is_told_once() {
        // The cluster has been asked for 30 GiB and the pod's filesystem is still the 20 GiB it had, full: the size
        // the claim asks for isn't room the session can use, so it's told to clean up (#479).
        let asked = Volume {
            requested: Some(30 << 30),
            ..settled(20 << 30)
        };
        let fake = Fake::new(asked, false);
        let first = on_report(&fake, "s", 20 << 30, 20 << 30, false, growth()).await;
        assert_eq!(
            first.notice.as_deref(),
            Some(
                "sloppod: the workspace is 100% full and the extra room its volume was asked for hasn't arrived \
                 yet; clean build output or caches."
            )
        );
        assert!(first.full);
        assert!(fake.grown().is_empty());

        // Five minutes later it's just as full and the room still hasn't arrived: nothing more is said.
        let next = on_report(&fake, "s", 20 << 30, 20 << 30, first.full, growth()).await;
        assert_eq!(next.notice, None);
        assert!(next.full);

        // Room to work in again — the filesystem grew, or work was cleaned up — so a workspace that fills up anew is
        // worth saying again.
        let grown = Fake::new(settled(30 << 30), false);
        let quiet = on_report(&grown, "s", 10 << 30, 30 << 30, next.full, growth()).await;
        assert_eq!(quiet.notice, None);
        assert!(!quiet.full);
        assert!(grown.grown().is_empty());
    }

    /// A session in the database, with a hub over it, for reports to be acted on (#479).
    async fn session(db: &PgPool) -> (Arc<Hub>, Uuid) {
        let id = Uuid::now_v7();
        db::insert_session(
            db,
            db::NewSessionRow {
                id,
                owner: "dev",
                title: "test",
                custom_title: false,
                pending_prompt: "",
                agent: "claude-code",
                kind: api::SessionKind::Work,
                review: api::ReviewPolicy::Off,
                model: None,
                repo: None,
                grants: &[],
            },
        )
        .await
        .unwrap();
        (
            Arc::new(Hub::new(Arc::new(crate::store::PgStore(db.clone())))),
            id,
        )
    }

    /// What the session's log was told about its workspace (#479).
    async fn notices(db: &PgPool, id: Uuid) -> Vec<Value> {
        db::events_before(db, id, i64::MAX, 100)
            .await
            .unwrap()
            .into_iter()
            .filter(|(_, _, payload)| payload["_meta"]["sloppod"]["notice"] == true)
            .map(|(_, _, payload)| payload)
            .collect()
    }

    /// What's waiting for the session's agent, which has none here (#479).
    async fn queued(db: &PgPool, id: Uuid) -> Vec<Value> {
        db::queues(db)
            .await
            .unwrap()
            .into_iter()
            .find(|(session, _)| *session == id)
            .map(|(_, queue)| queue)
            .unwrap_or_default()
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn two_reports_that_overlap_tell_a_session_once(db: PgPool) {
        let (hub, id) = session(&db).await;
        // Full, and a storage class that won't grow it: each of these reports is one that tells the session to clean
        // up. They're dealt with one at a time, so the second finds it already told.
        let fake = Arc::new(Fake::new(settled(20 << 30), true));
        let placement: Arc<dyn PlacementBackend> = fake.clone();
        let at = Instant::now();
        let report = |at: Instant| Report {
            at,
            used: 18 << 30,
            size: 20 << 30,
        };
        fake.holds_reads.store(true, Ordering::SeqCst);
        let first = tokio::spawn(on_disk(
            placement.clone(),
            db.clone(),
            hub.clone(),
            growth(),
            id,
            report(at),
        ));
        // The next report comes in while that one waits on the cluster, as one on a reconnect does.
        fake.reading.notified().await;
        let second = tokio::spawn(on_disk(
            placement.clone(),
            db.clone(),
            hub.clone(),
            growth(),
            id,
            report(at + Duration::from_millis(1)),
        ));
        fake.holds_reads.store(false, Ordering::SeqCst);
        fake.released.notify_one();
        first.await.unwrap();
        second.await.unwrap();

        // Said once, queued once, and it stands told that it can't grow.
        assert_eq!(notices(&db, id).await.len(), 1);
        assert_eq!(queued(&db, id).await.len(), 1);
        assert!(db::disk_full_told(&db, id).await.unwrap());
        assert!(fake.grown().is_empty());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_report_a_newer_one_has_superseded_is_dropped(db: PgPool) {
        let (hub, id) = session(&db).await;
        let fake = Arc::new(Fake::new(settled(20 << 30), true));
        let placement: Arc<dyn PlacementBackend> = fake.clone();
        let at = Instant::now();
        fake.holds_reads.store(true, Ordering::SeqCst);
        // The newer report — the workspace cleaned up since the one before it — is waiting on the cluster when the
        // older one starts, so the older one is dealt with after it and must not decide over it.
        let newer = tokio::spawn(on_disk(
            placement.clone(),
            db.clone(),
            hub.clone(),
            growth(),
            id,
            Report {
                at: at + Duration::from_millis(1),
                used: 2 << 30,
                size: 20 << 30,
            },
        ));
        fake.reading.notified().await;
        fake.holds_reads.store(false, Ordering::SeqCst);
        let older = tokio::spawn(on_disk(
            placement.clone(),
            db.clone(),
            hub.clone(),
            growth(),
            id,
            Report {
                at,
                used: 18 << 30,
                size: 20 << 30,
            },
        ));
        fake.released.notify_one();
        newer.await.unwrap();
        older.await.unwrap();

        // Nothing is said, and nothing stands told: the workspace isn't full any more, whatever the report that came
        // in before the newer one says.
        assert!(notices(&db, id).await.is_empty());
        assert!(queued(&db, id).await.is_empty());
        assert!(!db::disk_full_told(&db, id).await.unwrap());
    }

    /// Two reports that arrive in this order, with the older one's keeping held up (`told` is what the session stands
    /// told before either), and says what the session's log holds afterwards and what it stands told (#479). The
    /// newer one reaches the report path directly: the row lock holding the older one's keeping would hold its
    /// keeping too, and what matters here is when each report came in.
    async fn reports_that_arrive_around_a_held_up_keep(
        db: &PgPool,
        told: bool,
        older: (u64, u64),
        newer: (u64, u64),
    ) -> (usize, bool) {
        let (hub, id) = session(db).await;
        let fake = Arc::new(Fake::new(settled(20 << 30), true));
        let placement: Arc<dyn PlacementBackend> = fake.clone();
        db::set_disk_full_told(db, id, told).await.unwrap();
        // The session's row is held by this transaction, so the older report's keeping waits for it.
        let mut held = db.begin().await.unwrap();
        sqlx::query("select 1 from sessions where id = $1 for update")
            .bind(id)
            .execute(&mut *held)
            .await
            .unwrap();
        let older = crate::link::keep_disk(db, &hub, id, older.0, older.1);
        tokio::pin!(older);
        // Polled to its first await: it has taken the time it came in by, and is waiting on the row.
        assert!(futures::poll!(older.as_mut()).is_pending());

        on_disk(
            placement.clone(),
            db.clone(),
            hub.clone(),
            growth(),
            id,
            Report {
                at: Instant::now(),
                used: newer.0,
                size: newer.1,
            },
        )
        .await;
        held.rollback().await.unwrap();
        // What the link does once the report is kept: acted on, in the background.
        on_disk(
            placement,
            db.clone(),
            hub.clone(),
            growth(),
            id,
            older.await,
        )
        .await;
        (
            notices(db, id).await.len(),
            db::disk_full_told(db, id).await.unwrap(),
        )
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_report_held_up_while_a_newer_one_is_dealt_with_doesnt_decide_over_it(db: PgPool) {
        // Full, and a storage class that won't grow it, so this report is one that warns; the newer one, dealt with
        // while the older one's keeping waits, says the workspace was cleaned up. The older one is dropped: nothing
        // is said, and nothing stands told.
        assert_eq!(
            reports_that_arrive_around_a_held_up_keep(
                &db,
                false,
                (18 << 30, 20 << 30),
                (2 << 30, 20 << 30)
            )
            .await,
            (0, false)
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_report_held_up_while_a_newer_one_is_dealt_with_doesnt_undo_it(db: PgPool) {
        // The other way round: the session already stands told, the newer report says it's just as full, so nothing
        // more is said; the older quiet one, dealt with after it, doesn't clear what it was told.
        assert_eq!(
            reports_that_arrive_around_a_held_up_keep(
                &db,
                true,
                (2 << 30, 20 << 30),
                (18 << 30, 20 << 30)
            )
            .await,
            (0, true)
        );
    }
}
