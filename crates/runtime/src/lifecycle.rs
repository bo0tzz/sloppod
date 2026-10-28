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
