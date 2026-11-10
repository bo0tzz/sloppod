//! A session's life outside a request: placing its pod, parking and waking it, and removing it for good.

use std::collections::BTreeMap;

use serde_json::json;
use sloppod_contracts::capability;
use sloppod_contracts::{EgressSpec, Grant, PlacementSpec};

use crate::capabilities::Part;
use crate::memini::MeminiAdmin;
use crate::placement::{TOKEN_DIR, WORKDIR};
use crate::pods::SESSION_LABEL;
use crate::policies::SESSION_KEY;
use crate::session::ParkError;
use uuid::Uuid;

use crate::{AppState, db};

/// Whether a session without a memini key gets one. A sub-session's memory is what its parent gave it (ADR 27), so
/// it gets a key only for a memory grant it has.
fn mints_memory(session: &db::SessionRow, has_memory_grant: bool) -> bool {
    session.parent_id.is_none() || has_memory_grant
}

/// Whether a session of `kind` gets the owner's memory when they have memini. A utility session (ADR 26) only has
/// model access.
fn gets_memory(kind: sloppod_protocol::api::SessionKind) -> bool {
    kind != sloppod_protocol::api::SessionKind::Utility
}

/// Gives a session its memini key when its owner has memini and it has none, e.g. because it was just created, or
/// its key was lost (ADR 22). Keeps the namespace of an existing memory grant.
async fn ensure_memini_key(state: &AppState, session: &db::SessionRow) -> anyhow::Result<()> {
    if !gets_memory(session.kind) {
        return Ok(());
    }
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
    let granted = crate::capabilities::grant_with(&context, &crate::capabilities::Part::Memory)
        .and_then(|g| g.param("namespace"))
        .map(str::to_owned);
    if !mints_memory(session, granted.is_some()) {
        return Ok(());
    }
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

/// A session to start: what `start_session` needs besides its id.
pub struct NewSession<'a> {
    pub owner: &'a str,
    pub prompt: &'a str,
    pub title: &'a str,
    pub custom_title: bool,
    pub agent: &'a str,
    pub model: Option<&'a str>,
    pub review: sloppod_protocol::api::ReviewPolicy,
    pub repo: Option<&'a str>,
    pub grants: &'a [Grant],
    /// The session that spawned it, for a sub-session (ADR 27).
    pub parent: Option<Uuid>,
    /// The session whose workspace volume this session's starts as a copy of (#478), when its parent asked for one
    /// and the cluster can copy it.
    pub workspace_copied_from: Option<Uuid>,
}

/// Starts a work session: records it and its first message, and places its pod. A session whose pod can't be
/// placed is stopped, and the error says why.
pub async fn start_session(
    state: &AppState,
    id: Uuid,
    new: NewSession<'_>,
) -> anyhow::Result<db::SessionRow> {
    // The endpoint grant follows the model (#461): `new.model` was checked by `endpoint::chosen`.
    let mut grants = new.grants.to_vec();
    let endpoint = match crate::endpoint::EndpointModel::parse(new.model.unwrap_or_default()) {
        Some(model) => Some(
            crate::endpoint::session_grant(state, new.owner, &model)
                .await?
                .map_err(|why| anyhow::anyhow!(why))?,
        ),
        None => None,
    };
    crate::endpoint::follow_model(&mut grants, endpoint);
    db::insert_session(
        &state.db,
        db::NewSessionRow {
            id,
            owner: new.owner,
            title: new.title,
            custom_title: new.custom_title,
            pending_prompt: new.prompt,
            agent: new.agent,
            kind: sloppod_protocol::api::SessionKind::Work,
            review: new.review,
            model: new.model,
            repo: new.repo,
            grants: &grants,
        },
    )
    .await?;
    if let Some(parent) = new.parent {
        db::set_parent(&state.db, id, parent).await?;
    }
    // Recorded before its pod is placed: the copy is what the placement clones and what its supervisor takes over
    // (#478), until its agent has come up on it.
    if let Some(from) = new.workspace_copied_from {
        db::set_workspace_copied_from(&state.db, id, from).await?;
    }
    state
        .hub
        .record_user_message(
            id,
            &json!([{"type": "text", "text": new.prompt}]),
            false,
            None,
        )
        .await;
    let row = db::get_session(&state.db, id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session {id} vanished"))?;
    if let Err(e) = place(state, &row).await {
        state
            .hub
            .set_phase(id, sloppod_protocol::api::Phase::Stopped)
            .await;
        anyhow::bail!("starting the session pod: {e}");
    }
    Ok(row)
}

/// The egress policy a session's grants call for: the deployment's `direct` package hosts for a grant with an egress
/// part (`packages`), and every `intercepted` host through its listener.
pub fn egress_spec(
    session: &db::SessionRow,
    intercepted: Vec<String>,
    direct: &[String],
) -> EgressSpec {
    let id = session.id.to_string();
    EgressSpec {
        session_id: id.clone(),
        pod_labels: pod_labels(&id),
        intercepted_hosts: intercepted,
        direct_hosts: if crate::capabilities::holds(&session.context(), &Part::Egress) {
            direct.to_vec()
        } else {
            Vec::new()
        },
    }
}

/// Applies the egress policy an acting session's grants call for, from its grants and phase as they are when the
/// write's turn comes (ADR 11). Says whether it did: a session that isn't acting gets none. Run when its pod is
/// placed, and again when its grants change.
pub async fn apply_egress(state: &AppState, id: Uuid) -> anyhow::Result<bool> {
    state
        .hub
        .apply_egress(id, &*state.enforcer, |session| {
            egress_spec(
                session,
                state.policies.hosts_for(&session.context()),
                &state.config.direct_hosts,
            )
        })
        .await
}

/// After a session's grants changed (`what`, for the agent): its egress policy follows them, its agent and UIs are
/// told. A session that isn't acting gets its egress at its next placement.
pub async fn grants_changed(state: &AppState, id: Uuid, what: &str) {
    reapply_egress(state, id);
    crate::mcp::tools_changed(state, id).await;
    state.hub.broadcast_session(id).await;
    state
        .hub
        .notify(id, format!("sloppod: {what} See them with `list_grants`."))
        .await;
}

/// Applies an acting session's egress again after its grants changed, retrying until it lands: a revoked `packages`
/// must not keep its direct access because the control plane was briefly unreachable. Each attempt reads the session
/// afresh, so a retry, or an attempt that overlaps a later change, never lands an older set of grants after it. A
/// session that stops acting needs nothing more, and startup applies every acting session's egress again anyway.
fn reapply_egress(state: &AppState, id: Uuid) {
    let state = state.clone();
    tokio::spawn(async move {
        let mut wait = std::time::Duration::from_secs(5);
        while let Err(e) = apply_egress(&state, id).await {
            tracing::warn!(session = %id, error = %e, "applying egress after a grant change; trying again");
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(std::time::Duration::from_secs(60));
        }
    });
}

/// Removes the grants that have run out and tells their sessions; they already stopped counting when they ran out.
pub async fn sweep_expired_grants(state: &AppState) {
    let expired = match db::sweep_expired_grants(&state.db).await {
        Ok(expired) => expired,
        Err(e) => {
            tracing::warn!(error = %e, "removing grants that ran out");
            return;
        }
    };
    for (session, grant) in expired {
        let what = format!(
            "this session's grant ran out: {}",
            crate::grants::describe(&grant)
        );
        grants_changed(state, session, &what).await;
    }
}

/// Makes sure a session's pod and egress policy exist.
pub async fn place(state: &AppState, session: &db::SessionRow) -> anyhow::Result<()> {
    if let Err(e) = ensure_memini_key(state, session).await {
        tracing::warn!(session = %session.id, error = %e, "minting the session's memini key; no memory");
    }
    // What its endpoint grant needs, e.g. a key minted for it, before its agent asks (#461).
    state.providers.provision(&session.context()).await?;
    let id = session.id.to_string();
    let labels = BTreeMap::from([
        (SESSION_LABEL.to_owned(), id.clone()),
        ("app.kubernetes.io/part-of".to_owned(), "sloppod".to_owned()),
        (
            "app.kubernetes.io/component".to_owned(),
            "session".to_owned(),
        ),
    ]);
    // Read afresh, so it sees a memory grant minting just added. No pod goes up without its policy.
    if !apply_egress(state, session.id).await? {
        anyhow::bail!("session {id} stopped acting while it was being placed");
    }
    // Its policy is back, so it isn't released any more (#455).
    db::set_released(&state.db, session.id, false).await?;
    // A sub-session whose workspace starts as a copy of its parent's (#478): the parent's volume has to be there to
    // copy, and a copy that can't be made is forgotten rather than pretended.
    let copy = clone_from(state, session).await;
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
            storage_size: storage_size(state, session, copy).await,
            storage_class: state.config.session_storage_class.clone(),
            clone_from: copy.map(|parent| parent.to_string()),
        })
        .await?;
    Ok(())
}

/// Whether this session's workspace is placed as a copy of its parent's (#478), and which session that is: what its
/// row says, once the parent's volume is there to copy. A copy that can't be made now is forgotten, so the session
/// starts fresh and nothing later tries to take over a copy that isn't one.
async fn clone_from(state: &AppState, session: &db::SessionRow) -> Option<Uuid> {
    copy_from(
        &*state.placement,
        &state.db,
        session.id,
        session.workspace_copied_from,
    )
    .await
}

/// [`clone_from`]'s work, on the parts it needs, so a test can place a session with a backend that copies workspaces
/// and one that doesn't.
async fn copy_from(
    placement: &dyn sloppod_contracts::PlacementBackend,
    db: &sqlx::PgPool,
    id: Uuid,
    parent: Option<Uuid>,
) -> Option<Uuid> {
    let parent = parent?;
    match placement.can_copy_workspace(&parent.to_string()).await {
        Ok(true) => Some(parent),
        // A backend that can't copy, or a parent whose volume is gone (#478's fallback): the session starts fresh,
        // and its row is cleared so nothing later takes over a copy that was never made.
        Ok(false) => {
            if let Err(e) = db::clear_workspace_copied_from(db, id).await {
                tracing::warn!(session = %id, error = %e, "forgetting a workspace copy");
            }
            None
        }
        Err(e) => {
            tracing::warn!(session = %id, error = %e, "asking whether a workspace can be copied");
            None
        }
    }
}

/// How big the session's workspace volume starts: the configured size, and for a copy at least what the session it's
/// copied from has (#478) — a clone can't be smaller than its source, and the parent's claim is what the child's is
/// cloned from.
async fn storage_size(
    state: &AppState,
    session: &db::SessionRow,
    clone_from: Option<Uuid>,
) -> String {
    let configured = state.config.session_storage_size.clone();
    let Some(parent) = clone_from else {
        return configured;
    };
    let from = match state.placement.volume(&parent.to_string()).await {
        Ok(volume) => volume.and_then(|volume| volume.size()),
        Err(e) => {
            tracing::warn!(session = %session.id, error = %e, "reading the volume to copy a workspace from");
            None
        }
    };
    let (Some(configured_bytes), Some(from)) =
        (sloppod_contracts::storage_bytes(&configured), from)
    else {
        return configured;
    };
    if from <= configured_bytes {
        return configured;
    }
    tracing::info!(
        session = %session.id, from, "starting a copied workspace at the size it's copied from"
    );
    format!("{from}")
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
    state
        .hub
        .park(session.id, || suspend(state, session.id))
        .await
}

/// Parks a session like [`park`], if it's idle with nothing going on ([`Hub::park_quiet`]); says whether it did.
///
/// [`Hub::park_quiet`]: crate::session::Hub::park_quiet
pub async fn park_quiet(state: &AppState, session: &db::SessionRow) -> Result<bool, ParkError> {
    state
        .hub
        .park_quiet(session.id, || suspend(state, session.id))
        .await
}

async fn suspend(state: &AppState, id: Uuid) -> anyhow::Result<()> {
    state.placement.suspend(&id.to_string()).await?;
    // A parked session holds no listener; waking applies its policy again. Startup releases what's left here.
    // It's parked already, so no apply after this one gives its pod egress again.
    if let Err(e) = state.hub.release_egress(id, &*state.enforcer).await {
        tracing::warn!(session = %id, error = %e, "releasing a parked session's egress");
    }
    Ok(())
}

/// How long a session has to have been quiet before its pod is replaced for a newer session image.
const ROLL_AFTER_IDLE: chrono::Duration = chrono::Duration::minutes(15);
/// The same, for a session whose supervisor doesn't say whether the agent has commands running (one from before it
/// could), so it's given longer for background work to finish.
const ROLL_AFTER_IDLE_UNKNOWN: chrono::Duration = chrono::Duration::hours(2);
/// The same, for a session whose agent was asked to make room for the image (#416): long enough that the supervisor's
/// count of the agent's commands covers anything the last turn started in the background.
const ROLL_AFTER_NUDGE: chrono::Duration =
    chrono::Duration::seconds(3 * sloppod_protocol::link::COMMANDS_EVERY.as_secs() as i64);

/// How long a session's pod may run an older image before its agent is asked to find a quiet moment (#230).
const NUDGE_AFTER: chrono::Duration = chrono::Duration::hours(24);

/// Whether a session that can't be given a new pod yet should be asked to find a quiet moment: its pod has run an
/// older image for long enough. The notice only goes while no turn is running, so it interrupts none.
fn due_for_nudge(
    pod_made: Option<chrono::DateTime<chrono::Utc>>,
    running: Option<&str>,
    current: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    running.is_some_and(|image| image != current)
        && pod_made.is_some_and(|made| now - made >= NUDGE_AFTER)
}

const NUDGE: &str = "sloppod: a newer session image is out, and this session's pod has run the old one for over a \
                     day. The pod is replaced with a new one the next time the session is quiet: idle, with nothing \
                     queued and none of your commands running. When it suits your work, wrap up or stop what you \
                     have running in the background, so that can happen. Anything still running then is ended, and \
                     you're told what was.";

/// Whether a session's pod is due to be replaced for the current session image: it runs another, and the session
/// is quiet enough that the replacement interrupts nothing. `busy` is [`Hub::busy`](crate::session::Hub::busy).
/// A session whose agent was asked to make room for this image is replaced as soon as it's quiet (#416), once its
/// supervisor has had time to report what the last turn left running: a busy one gets a notice every few minutes, so
/// it's never quiet for long. Others have to have been quiet for a while, as their agent might be about to pick work
/// up again.
fn due_for_image(
    session: &db::SessionRow,
    running: Option<&str>,
    current: &str,
    busy: Option<bool>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    let nudged = session.image_nudged.as_deref() == Some(current);
    let quiet_for = match busy {
        Some(true) => return false,
        Some(false) if nudged => ROLL_AFTER_NUDGE,
        Some(false) => ROLL_AFTER_IDLE,
        None => ROLL_AFTER_IDLE_UNKNOWN,
    };
    session.phase() == sloppod_protocol::api::Phase::Idle
        && now - session.active_at >= quiet_for
        && running.is_some_and(|image| image != current)
}

/// How long a copied workspace's volume may go unprovisioned before the runtime gives up on the copy and places the
/// session fresh (#478). A clone the storage class takes is provisioned in seconds; one it doesn't stays pending
/// forever, and the session would wait on a pod that can never start.
const COPY_MAY_TAKE: chrono::Duration = chrono::Duration::minutes(2);

/// Whether a copied workspace's volume never arrived (#478): the claim is there, nothing has been provisioned for it,
/// and the session has been starting long enough that a clone the cluster takes would have been. A claim that isn't
/// there yet isn't this — the placement is still making it.
fn copy_stuck(
    started: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
    volume: Option<&sloppod_contracts::Volume>,
) -> bool {
    match volume {
        Some(volume) => volume.capacity.is_none() && now - started >= COPY_MAY_TAKE,
        None => false,
    }
}

/// Gives up on a session's copied workspace (#478) and gives back the session as it now is — its row asks for no
/// copy, so the placement that follows asks for a fresh volume — or nothing when there's nothing to give up on. Its
/// objects are removed, the Sandbox taking the unprovisioned claim with it; a removal that fails is tried again by
/// the next sweep, since the session waits for a fresh placement either way.
async fn drop_copy(
    placement: &dyn sloppod_contracts::PlacementBackend,
    db: &sqlx::PgPool,
    session: &db::SessionRow,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<db::SessionRow> {
    let id = session.id;
    let volume = match placement.volume(&id.to_string()).await {
        Ok(volume) => volume,
        Err(e) => {
            tracing::warn!(session = %id, error = %e, "reading a copied workspace's volume");
            return None;
        }
    };
    if !copy_stuck(session.created_at, now, volume.as_ref()) {
        return None;
    }
    tracing::warn!(
        session = %id, from = ?session.workspace_copied_from,
        "a copied workspace never provisioned; placing a fresh one"
    );
    if let Err(e) = db::drop_workspace_copy(db, id).await {
        tracing::warn!(session = %id, error = %e, "giving up on a workspace copy that never provisioned");
        return None;
    }
    if let Err(e) = placement.delete(&id.to_string()).await {
        tracing::warn!(session = %id, error = %e, "removing a copied session's objects; the next sweep tries again");
    }
    // Read back rather than handed on: the row is what says what the next placement asks for.
    match db::get_session(db, id).await {
        Ok(Some(row)) => Some(row),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(session = %id, error = %e, "reading a session after giving up on its copied workspace");
            None
        }
    }
}

/// Where a session whose copied workspace was given up on stands (#478): what the copy left is still being removed,
/// or a fresh workspace is being brought up.
pub const COPY_DROPPING: &str = "dropping";
pub const COPY_PLACING: &str = "placing";

/// Carries a session whose copied workspace was given up on a step further (#478), and says whether it's done with
/// the fallback.
///
/// While it's `dropping`, what the copy left is removed — asked for again by every sweep — and nothing is placed: a
/// placement applied to a Sandbox that's on its way out goes with it, and the session with it. Once the cluster says
/// it's all gone the session moves to `placing`, which is recorded before the placement so a failure or a restart
/// during it is retried without removing anything again. In `placing` the placement is applied — again is fine, it's
/// an apply, and a restart's reconciliation may have placed the session already — and the stage is cleared. The
/// removal is never repeated from `placing` on, so the volume a retry sees is the session's own.
async fn advance_dropped_copy<F, Fut>(
    placement: &dyn sloppod_contracts::PlacementBackend,
    db: &sqlx::PgPool,
    session: &db::SessionRow,
    place: F,
) -> bool
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    let id = session.id;
    if session.workspace_copy_stage.as_deref() != Some(COPY_PLACING) {
        match placement.removed(&id.to_string()).await {
            Ok(true) => {
                if let Err(e) = db::begin_workspace_placing(db, id).await {
                    tracing::warn!(session = %id, error = %e, "moving on to placing a session fresh");
                    return false;
                }
            }
            // Still there, or still being deleted: asked for again, and the next sweep looks.
            Ok(false) => {
                if let Err(e) = placement.delete(&id.to_string()).await {
                    tracing::warn!(session = %id, error = %e, "removing what a copied workspace left");
                }
                return false;
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "reading what a copied workspace left");
                return false;
            }
        }
    }
    if let Err(e) = place().await {
        tracing::warn!(session = %id, error = %e, "placing a session whose workspace copy was given up on");
        return false;
    }
    match db::finish_workspace_fresh(db, id).await {
        Ok(()) => true,
        Err(e) => {
            // Placed, but the row still waits: the next sweep places it again, which the placement takes, and
            // removes nothing — the volume there is the session's own by now.
            tracing::warn!(session = %id, error = %e, "finishing a fresh placement after a dropped workspace copy");
            false
        }
    }
}

/// Gives up on the copies that never provisioned (#478) — the storage class doesn't clone, or the clone keeps
/// failing — and places those sessions fresh, telling them: better a fresh workspace they know about than a pod that
/// never starts. Each step is retried until it lands, so a removal or a placement that fails doesn't leave a session
/// with no objects and nothing looking for it.
pub async fn sweep_copied_workspaces(state: &AppState) {
    let copied = match db::copied_workspaces(&state.db).await {
        Ok(copied) => copied,
        Err(e) => {
            tracing::warn!(error = %e, "listing sessions whose workspace is a copy");
            return;
        }
    };
    let now = chrono::Utc::now();
    for (id, _) in copied {
        let Ok(Some(session)) = db::get_session(&state.db, id).await else {
            continue;
        };
        if drop_copy(&*state.placement, &state.db, &session, now)
            .await
            .is_some()
        {
            tracing::info!(session = %id, "gave up on a copied workspace that never provisioned");
        }
    }
    // Read after the pass above, so a copy dropped just now is placed in this same sweep.
    let waiting = match db::copies_awaiting_fresh(&state.db).await {
        Ok(waiting) => waiting,
        Err(e) => {
            tracing::warn!(error = %e, "listing sessions waiting to be placed fresh");
            return;
        }
    };
    for (id, _) in waiting {
        let Ok(Some(session)) = db::get_session(&state.db, id).await else {
            continue;
        };
        if !advance_dropped_copy(&*state.placement, &state.db, &session, || {
            place(state, &session)
        })
        .await
        {
            continue;
        }
        tracing::info!(session = %id, "placed a session fresh: its copied workspace never provisioned");
        state
            .hub
            .notify_or_record(
                id,
                "Workspace",
                "sloppod: this session's workspace couldn't be copied from its parent's, so it starts with a fresh \
                 one; its tools and caches are installed again."
                    .to_owned(),
            )
            .await;
    }
}

/// Gives idle sessions whose pods run an older session image a new pod (#28). The pod is deleted and the Sandbox's
/// controller starts a new one from the current spec, the same path an evicted pod takes: the supervisor reports and
/// syncs on its way out, and the agent resumes its conversation in the new pod.
pub async fn roll_images(state: &AppState) {
    // A pod replaced now would take room one that can't be placed is waiting for, or wait itself (#454).
    if crate::room::short_of_room(&state.placement.unschedulable()) {
        return;
    }
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
            // Recorded only once it's sent: a session busy with a turn now is asked at a later sweep.
            if due_for_nudge(
                pod.and_then(|pod| pod.made),
                running.as_deref(),
                current,
                now,
            ) && session.image_nudged.as_deref() != Some(current.as_str())
                && state
                    .hub
                    .notify_when_idle(session.id, None, NUDGE.to_owned())
                    .await
                && let Err(e) = db::set_image_nudged(&state.db, session.id, current).await
            {
                tracing::warn!(session = %session.id, error = %e, "recording that a session was asked to make room for an image");
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
/// left as they are, and a deleted one stays parked until it's restored. `why` is logged, so a wake can be traced to
/// what asked for it.
pub async fn wake(state: &AppState, session: &db::SessionRow, why: &str) -> anyhow::Result<()> {
    state
        .hub
        .wake(session.id, || async {
            tracing::info!(session = %session.id, why, "waking a parked session");
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
    state.hub.stop(id).await?;
    state.tool_streams.forget(id);
    state.placement.delete(&id.to_string()).await?;
    state.hub.remove_egress(id, &*state.enforcer).await?;
    remove_keys(state, session).await
}

/// Removes what model providers made for sessions that don't hold it any more (#461): a virtual key whose release
/// failed, or whose session was purged while the runtime was down. A session keeps its endpoint key until it stops or
/// is purged, parked or deleted-and-restorable too.
pub async fn sweep_endpoint_keys(state: &AppState) {
    let held = match db::endpoint_grants_held(&state.db).await {
        Ok(held) => held,
        Err(e) => {
            tracing::warn!(error = %e, "listing the endpoint grants sessions hold");
            return;
        }
    };
    state.providers.sweep(&held).await;
}

/// Deletes the keys a session's pod uses: its memini key, its own key (#455), and its endpoint's key (#461). Placing
/// the session mints them again.
pub async fn remove_keys(state: &AppState, session: &db::SessionRow) -> anyhow::Result<()> {
    let id = session.id;
    if crate::capabilities::holds(&session.context(), &crate::capabilities::Part::Memory)
        && let Some((admin, _)) = state.credentials.memini(&session.owner).await?
        && let Err(e) = admin
            .delete_key(&MeminiAdmin::key_name(&id.to_string()))
            .await
    {
        tracing::warn!(session = %id, error = %e, "deleting the session's memini key");
    }
    // What it spent since the last read, before its key goes (#461).
    crate::spend::read_last(state, &session.context()).await;
    state
        .providers
        .release(&id.to_string(), &session.grants())
        .await?;
    state
        .credentials
        .delete_session_secret(id, SESSION_KEY)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// A workspace copied from another session's is given up on only once its claim is there and has stayed
    /// unprovisioned well past the time a clone the storage class takes would have (#478).
    #[test]
    fn a_copied_workspace_that_never_provisions_is_placed_fresh() {
        let now = chrono::Utc::now();
        let volume = |capacity: Option<u64>| sloppod_contracts::Volume {
            name: "workdir-sloppod-x".into(),
            session_id: Some("x".into()),
            capacity,
            requested: Some(20 << 30),
            resizing: false,
        };
        // Provisioned, on its way out, or not made yet: left alone.
        assert!(!super::copy_stuck(
            now - chrono::Duration::minutes(30),
            now,
            Some(&volume(Some(20 << 30)))
        ));
        assert!(!super::copy_stuck(
            now - chrono::Duration::minutes(30),
            now,
            None
        ));
        // There, unprovisioned, and young: the clone may still be arriving.
        assert!(!super::copy_stuck(
            now - chrono::Duration::seconds(30),
            now,
            Some(&volume(None))
        ));
        // There, unprovisioned, and old enough: the cluster isn't going to.
        assert!(super::copy_stuck(
            now - chrono::Duration::minutes(5),
            now,
            Some(&volume(None))
        ));
        assert!(super::copy_stuck(
            now - super::COPY_MAY_TAKE,
            now,
            Some(&volume(None))
        ));
    }

    /// A backend that copies workspaces, or one that can't, for the placement of a copied one (#478).
    struct Copies(bool);

    #[async_trait::async_trait]
    impl sloppod_contracts::PlacementBackend for Copies {
        fn manifest(&self) -> sloppod_contracts::Manifest {
            sloppod_contracts::Manifest::builtin("copies")
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
        async fn can_copy_workspace(&self, _from: &str) -> sloppod_contracts::Result<bool> {
            Ok(self.0)
        }
    }

    /// A placement backend whose session volume is what a test says, and which records the sessions whose objects it
    /// was asked to remove (#478).
    struct Claims {
        volume: Option<sloppod_contracts::Volume>,
        /// Whether the backend holds nothing for the session any more — its pod included, so a claim that's gone
        /// while its Sandbox is still terminating is not this (#478).
        removed: bool,
        deleted: std::sync::Mutex<Vec<String>>,
    }

    impl Claims {
        /// A backend whose session has this claim, and nothing else left when it has none.
        fn new(volume: Option<sloppod_contracts::Volume>) -> Self {
            Self {
                removed: volume.is_none(),
                volume,
                deleted: Default::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl sloppod_contracts::PlacementBackend for Claims {
        fn manifest(&self) -> sloppod_contracts::Manifest {
            sloppod_contracts::Manifest::builtin("claims")
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
        async fn delete(&self, session_id: &str) -> sloppod_contracts::Result<()> {
            self.deleted.lock().unwrap().push(session_id.to_owned());
            Ok(())
        }
        async fn volume(
            &self,
            _session_id: &str,
        ) -> sloppod_contracts::Result<Option<sloppod_contracts::Volume>> {
            Ok(self.volume.clone())
        }
        async fn removed(&self, _session_id: &str) -> sloppod_contracts::Result<bool> {
            Ok(self.removed)
        }
    }

    /// A session whose workspace is a copy of `parent`'s, for the sweeps that place it fresh (#478).
    async fn copied_session(
        db: &sqlx::PgPool,
        child: uuid::Uuid,
        parent: uuid::Uuid,
    ) -> crate::db::SessionRow {
        crate::db::insert_session(
            db,
            crate::db::NewSessionRow {
                id: child,
                owner: "dev",
                title: "test",
                custom_title: false,
                pending_prompt: "",
                agent: "claude-code",
                kind: sloppod_protocol::api::SessionKind::Work,
                review: sloppod_protocol::api::ReviewPolicy::Off,
                model: None,
                repo: None,
                grants: &[],
            },
        )
        .await
        .unwrap();
        crate::db::set_workspace_copied_from(db, child, parent)
            .await
            .unwrap();
        crate::db::get_session(db, child).await.unwrap().unwrap()
    }

    /// A copied workspace the cluster never provisions is given up on — the copy forgotten and the session's objects
    /// removed so it can be placed fresh — while one that has provisioned, one whose claim is still on its way, and a
    /// session that has only just started are left alone (#478).
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_copied_workspace_that_never_arrives_is_given_up_on(db: sqlx::PgPool) {
        let parent = uuid::Uuid::new_v4();
        let child = uuid::Uuid::now_v7();
        let row = copied_session(&db, child, parent).await;
        let claim = |capacity: Option<u64>| sloppod_contracts::Volume {
            name: format!("workdir-sloppod-{child}"),
            session_id: Some(child.to_string()),
            capacity,
            requested: Some(20 << 30),
            resizing: false,
        };

        // The claim is there, unprovisioned, and the session has only just started: the clone may still be arriving.
        let waiting = Claims::new(Some(claim(None)));
        assert!(
            super::drop_copy(
                &waiting,
                &db,
                &row,
                row.created_at + chrono::Duration::seconds(30)
            )
            .await
            .is_none()
        );
        assert!(waiting.deleted.lock().unwrap().is_empty());
        assert_eq!(
            crate::db::get_session(&db, child)
                .await
                .unwrap()
                .unwrap()
                .workspace_copied_from,
            Some(parent)
        );

        // Minutes later, with nothing provisioned: given up on, its objects removed, and the copy forgotten so the
        // placement that follows asks for a fresh volume.
        assert!(
            super::drop_copy(
                &waiting,
                &db,
                &row,
                row.created_at + chrono::Duration::minutes(5)
            )
            .await
            .is_some()
        );
        assert_eq!(waiting.deleted.lock().unwrap().len(), 1);
        // The row it gives back is the one to carry on from: no copy on it, and what the copy left on its way out.
        let waiting_row = crate::db::get_session(&db, child).await.unwrap().unwrap();
        assert_eq!(waiting_row.workspace_copied_from, None);
        assert_eq!(
            waiting_row.workspace_copy_stage.as_deref(),
            Some(super::COPY_DROPPING)
        );

        // A claim the cluster did provision, however old the session: left alone.
        let provisioned = Claims::new(Some(claim(Some(20 << 30))));
        assert!(
            super::drop_copy(
                &provisioned,
                &db,
                &row,
                row.created_at + chrono::Duration::hours(1)
            )
            .await
            .is_none()
        );
        assert!(provisioned.deleted.lock().unwrap().is_empty());

        // No claim at all yet — the placement is still making one — is left alone too.
        let none = Claims::new(None);
        assert!(
            super::drop_copy(
                &none,
                &db,
                &row,
                row.created_at + chrono::Duration::hours(1)
            )
            .await
            .is_none()
        );
    }

    /// A session waiting for a fresh placement after its copy was given up on keeps waiting until everything lands:
    /// what the copy left is removed again and the placement is tried again, sweep after sweep, rather than the
    /// session being left with no objects and nothing looking for it (#478).
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_given_up_copy_is_retried_until_the_session_is_placed_fresh(db: sqlx::PgPool) {
        let parent = uuid::Uuid::new_v4();
        let child = uuid::Uuid::now_v7();
        let row = copied_session(&db, child, parent).await;
        let volume = |capacity| sloppod_contracts::Volume {
            name: format!("workdir-sloppod-{child}"),
            session_id: Some(child.to_string()),
            capacity,
            requested: Some(20 << 30),
            resizing: false,
        };
        let stage = |db: &sqlx::PgPool| {
            let db = db.clone();
            async move {
                crate::db::get_session(&db, child)
                    .await
                    .unwrap()
                    .unwrap()
                    .workspace_copy_stage
            }
        };

        // The copy never provisioned and the sweep gives up on it: the copy is forgotten, what it left is asked for,
        // and the session waits in the `dropping` stage.
        let left = Claims::new(Some(volume(None)));
        let dropped = super::drop_copy(
            &left,
            &db,
            &row,
            row.created_at + chrono::Duration::minutes(5),
        )
        .await
        .expect("a copy that never provisioned is given up on");
        assert_eq!(dropped.workspace_copied_from, None);
        assert_eq!(
            dropped.workspace_copy_stage.as_deref(),
            Some(super::COPY_DROPPING)
        );
        assert_eq!(left.deleted.lock().unwrap().len(), 1);

        // The claim is still there: nothing is placed, and the removal is asked for again.
        assert!(
            !super::advance_dropped_copy(&left, &db, &dropped, || async { Ok(()) }).await,
            "a session whose claim is still there isn't placed yet"
        );
        assert_eq!(left.deleted.lock().unwrap().len(), 2);
        assert_eq!(stage(&db).await.as_deref(), Some(super::COPY_DROPPING));

        // The claim is gone but its Sandbox is still terminating: still nothing placed, and the removal asked for
        // again — a placement applied to a Sandbox on its way out goes with it (#478).
        let terminating = Claims {
            volume: None,
            removed: false,
            deleted: Default::default(),
        };
        assert!(
            !super::advance_dropped_copy(&terminating, &db, &dropped, || async { Ok(()) }).await,
            "a session whose Sandbox is still going isn't placed yet"
        );
        assert_eq!(terminating.deleted.lock().unwrap().len(), 1);
        assert_eq!(stage(&db).await.as_deref(), Some(super::COPY_DROPPING));

        // All of it is gone and the placement fails: the session moves on to `placing` and waits there, so the next
        // sweep places it without removing anything again.
        let gone = Claims::new(None);
        assert!(
            !super::advance_dropped_copy(&gone, &db, &dropped, || async {
                anyhow::bail!("the cluster wouldn't place it")
            })
            .await
        );
        assert_eq!(stage(&db).await.as_deref(), Some(super::COPY_PLACING));

        // A restart in the middle of that: the reconciliation has placed the session, so its fresh volume is there.
        // Carrying on from the session as it now is — which is what a sweep reads — places it again, an apply, and
        // removes nothing, since the volume it sees is the session's own now.
        let placing = crate::db::get_session(&db, child).await.unwrap().unwrap();
        assert_eq!(
            placing.workspace_copy_stage.as_deref(),
            Some(super::COPY_PLACING)
        );
        let fresh = Claims::new(Some(volume(Some(20 << 30))));
        let placed = std::sync::atomic::AtomicBool::new(false);
        assert!(
            super::advance_dropped_copy(&fresh, &db, &placing, || async {
                placed.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
            .await
        );
        assert!(placed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            fresh.deleted.lock().unwrap().is_empty(),
            "the session's own volume is never removed"
        );
        assert_eq!(stage(&db).await, None);
    }

    /// A child whose parent asked for a copy gets one where the backend can make it, and where it can't — a backend
    /// that doesn't copy, or a parent's volume that's gone — it starts fresh and its row no longer says otherwise, so
    /// nothing later tries to take over a copy that was never made (#478).
    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_copy_a_backend_cannot_make_leaves_the_session_fresh(db: sqlx::PgPool) {
        let parent = uuid::Uuid::new_v4();
        let child = uuid::Uuid::now_v7();
        crate::db::insert_session(
            &db,
            crate::db::NewSessionRow {
                id: child,
                owner: "dev",
                title: "test",
                custom_title: false,
                pending_prompt: "",
                agent: "claude-code",
                kind: sloppod_protocol::api::SessionKind::Work,
                review: sloppod_protocol::api::ReviewPolicy::Off,
                model: None,
                repo: None,
                grants: &[],
            },
        )
        .await
        .unwrap();
        crate::db::set_workspace_copied_from(&db, child, parent)
            .await
            .unwrap();

        let copied = super::copy_from(&Copies(true), &db, child, Some(parent)).await;
        assert_eq!(copied, Some(parent));
        let row = crate::db::get_session(&db, child).await.unwrap().unwrap();
        assert_eq!(row.workspace_copied_from, Some(parent));

        // A backend that can't copy: fresh, and the row says nothing of a copy.
        let refused = super::copy_from(&Copies(false), &db, child, Some(parent)).await;
        assert_eq!(refused, None);
        let row = crate::db::get_session(&db, child).await.unwrap().unwrap();
        assert_eq!(row.workspace_copied_from, None);

        // A session that never asked for a copy isn't one.
        assert_eq!(
            super::copy_from(&Copies(true), &db, child, None).await,
            None
        );
    }

    #[test]
    fn a_sub_session_gets_memory_only_if_its_parent_gave_it() {
        let mut session = crate::db::SessionRow::for_test(uuid::Uuid::new_v4(), "alice");
        assert!(
            super::mints_memory(&session, false),
            "a session of its own gets memory"
        );
        session.parent_id = Some(uuid::Uuid::new_v4());
        assert!(!super::mints_memory(&session, false));
        assert!(super::mints_memory(&session, true));
    }

    #[test]
    fn a_utility_session_gets_no_memory() {
        use sloppod_protocol::api::SessionKind;
        assert!(gets_memory(SessionKind::Work));
        assert!(gets_memory(SessionKind::Review));
        assert!(!gets_memory(SessionKind::Utility));
    }

    #[test]
    fn a_session_long_on_an_old_image_is_nudged_once() {
        let now = chrono::Utc::now();
        let day_ago = Some(now - chrono::Duration::hours(25));
        assert!(due_for_nudge(
            day_ago,
            Some("session:old"),
            "session:new",
            now
        ));
        // Not before a day, not on the current image, not without a pod.
        assert!(!due_for_nudge(
            Some(now - chrono::Duration::hours(2)),
            Some("session:old"),
            "session:new",
            now
        ));
        assert!(!due_for_nudge(
            day_ago,
            Some("session:new"),
            "session:new",
            now
        ));
        assert!(!due_for_nudge(day_ago, None, "session:new", now));
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

    #[test]
    fn a_session_asked_to_make_room_gets_a_new_pod_as_soon_as_it_is_quiet() {
        let now = Utc::now();
        let session = |nudged: Option<&str>, quiet_minutes: i64| {
            let mut row = db::SessionRow::for_test(Uuid::now_v7(), "dev");
            row.phase = Phase::Idle.as_str().to_owned();
            row.active_at = now - Duration::minutes(quiet_minutes);
            row.image_nudged = nudged.map(str::to_owned);
            row
        };
        let due = |row: &db::SessionRow, busy: Option<bool>| {
            due_for_image(row, Some("session:old"), "session:new", busy, now)
        };
        // Asked for this image: quiet for a minute is enough.
        assert!(due(&session(Some("session:new"), 1), Some(false)));
        // Not right after a turn, whose background commands the supervisor may not have counted yet (it reports
        // every 20 seconds), nor while anything is going on, nor when its commands aren't known.
        assert!(!due(&session(Some("session:new"), 0), Some(false)));
        assert!(!due(&session(Some("session:new"), 1), Some(true)));
        assert!(!due(&session(Some("session:new"), 1), None));
        // Never asked, or asked for an older image: it has to have been quiet a while.
        assert!(!due(&session(None, 0), Some(false)));
        assert!(!due(&session(Some("session:older"), 0), Some(false)));
        assert!(due(&session(Some("session:older"), 20), Some(false)));
    }

    /// A request policy that only names its hosts.
    struct Hosts(&'static [&'static str]);

    #[async_trait::async_trait]
    impl sloppod_contracts::RequestPolicy for Hosts {
        fn manifest(&self) -> sloppod_contracts::Manifest {
            sloppod_contracts::Manifest::builtin("hosts")
        }

        fn hosts(&self) -> Vec<String> {
            self.0.iter().map(|h| (*h).to_owned()).collect()
        }

        async fn decide(
            &self,
            _: &sloppod_contracts::SessionContext,
            _: &sloppod_contracts::HttpRequest,
        ) -> sloppod_contracts::Result<sloppod_contracts::Decision> {
            Ok(sloppod_contracts::Decision::allow())
        }
    }

    #[test]
    fn a_session_reaches_only_the_hosts_of_the_parts_it_holds() {
        use crate::capabilities::{Part, Provider};
        use sloppod_contracts::{Grant, capability};
        use std::sync::Arc;

        let mut policies = crate::policies::Policies::default();
        policies.register(
            Part::Model(Provider::Anthropic),
            Arc::new(Hosts(&["api.anthropic.com"])),
        );
        policies.register(
            Part::Model(Provider::ChatGpt),
            Arc::new(Hosts(&["chatgpt.com"])),
        );
        policies.register(
            Part::Github,
            Arc::new(Hosts(&["api.github.com", "github.com"])),
        );
        let direct = ["crates.io".to_owned()];
        let spec = |grants: &[&str]| {
            let mut session = crate::db::SessionRow::for_test(uuid::Uuid::new_v4(), "dev");
            let grants: Vec<Grant> = grants
                .iter()
                .map(|name| match *name {
                    capability::REPO => {
                        Grant::new(name, serde_json::json!({"repo": "o/r", "access": "rw"}))
                    }
                    _ => Grant::new(name, serde_json::json!({})),
                })
                .collect();
            session.set_grants(&grants);
            let spec =
                super::egress_spec(&session, policies.hosts_for(&session.context()), &direct);
            (spec.intercepted_hosts, spec.direct_hosts)
        };

        assert_eq!(
            spec(&[capability::MODEL, capability::DOCS]),
            (vec!["api.anthropic.com".to_owned()], vec![])
        );
        assert_eq!(
            spec(&[capability::MODEL, capability::GITHUB, capability::REPO]),
            (
                vec![
                    "api.anthropic.com".to_owned(),
                    "api.github.com".to_owned(),
                    "github.com".to_owned()
                ],
                vec![]
            )
        );
        // A repository without github reaches no GitHub host: the repository is a param of github.
        assert_eq!(
            spec(&[capability::MODEL, capability::REPO]),
            (vec!["api.anthropic.com".to_owned()], vec![])
        );
        assert_eq!(
            spec(&[capability::CODEX, capability::PACKAGES]),
            (vec!["chatgpt.com".to_owned()], vec!["crates.io".to_owned()])
        );
        assert_eq!(spec(&[]), (vec![], vec![]));
        // The listeners' certificate still covers every policy's hosts.
        assert_eq!(policies.hosts().len(), 4);
    }
}
