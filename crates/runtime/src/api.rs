//! REST API (`docs/api.md`).

use std::collections::BTreeMap;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use serde::Deserialize;
use serde_json::json;
use sloppod_contracts::{EgressSpec, Grant, PlacementSpec};
use sloppod_protocol::api::{
    Decision, NewCredential, NewSession, NewStandingApproval, Phase, ProposalStatus, ReviewPolicy,
    SessionKind,
};
use uuid::Uuid;

use crate::credentials::CHATGPT;
use crate::memini::MeminiAdmin;
use crate::placement::{TOKEN_DIR, WORKDIR};
use crate::pods::SESSION_LABEL;
use crate::policies::SESSION_KEY;
use crate::proposals::{self, Verdict};
use crate::session::ParkError;
use crate::users::User;
use crate::{AppState, db, reviews};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/me", get(me))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/repos", post(add_repo))
        .route("/sessions/deleted", get(list_deleted_sessions))
        .route("/sessions/{id}/restore", post(restore_session))
        .route("/sessions/{id}/park", post(park_session))
        .route("/sessions/{id}/wake", post(wake_session))
        .route("/sessions/{id}/review", put(set_review))
        .route("/repos", get(repos))
        .route(
            "/credentials",
            get(list_credentials).post(create_credential),
        )
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/default", post(default_credential))
        .route(
            "/standing-approvals",
            get(list_standing_approvals).post(create_standing_approval),
        )
        .route("/standing-approvals/{id}", delete(revoke_standing_approval))
        .route("/proposals", get(list_proposals))
        .route("/proposals/{id}", get(get_proposal))
        .route("/proposals/{id}/reviews", get(list_reviews))
        .route("/proposals/{id}/approve", post(approve))
        .route("/proposals/{id}/reject", post(reject))
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl<E: std::fmt::Display> From<E> for ApiError {
    fn from(e: E) -> Self {
        tracing::error!(error = %e, "API error");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

/// A session that's in a turn or deleted can't be parked: a conflict, not a failure.
fn park_error(e: ParkError) -> ApiError {
    match e {
        ParkError::InTurn | ParkError::Stopped => ApiError(StatusCode::CONFLICT, e.to_string()),
        ParkError::Failed(e) => e.into(),
    }
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found".into())
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}

type ApiResult<T> = Result<T, ApiError>;

async fn me(User(user): User) -> impl IntoResponse {
    Json(json!({"user": user}))
}

/// The models the agent offered, from a `configOptions` list; options may be grouped.
fn offered_models(agent_state: &serde_json::Value) -> Vec<serde_json::Value> {
    let options = agent_state["configOptions"]
        .as_array()
        .into_iter()
        .flatten();
    let Some(model) = options.into_iter().find(|o| o["id"] == "model") else {
        return Vec::new();
    };
    model["options"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|o| match o["options"].as_array() {
            Some(group) => group.clone(),
            None => vec![o.clone()],
        })
        .filter(|o| o["value"].is_string())
        .map(|o| json!({"value": o["value"], "name": o["name"].as_str().or(o["value"].as_str())}))
        .collect()
}

async fn settings(state: &AppState, user: &str) -> ApiResult<serde_json::Value> {
    let model = db::user_model(&state.db, user).await?;
    let models = db::latest_agent_state(&state.db, user)
        .await?
        .map(|s| offered_models(&s))
        .unwrap_or_default();
    let review = db::user_review(&state.db, user).await?;
    Ok(json!({"model": model, "models": models, "review": review}))
}

/// Deserializes a field that's present, so `Option<Option<T>>` tells an absent field from a null one.
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

fn review_policy(review: &str) -> ApiResult<ReviewPolicy> {
    review.parse().map_err(|_| {
        let policies: Vec<_> = ReviewPolicy::ALL.iter().map(|p| p.as_str()).collect();
        bad_request(format!("review must be one of {}", policies.join(", ")))
    })
}

async fn get_settings(
    State(state): State<AppState>,
    User(user): User,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(settings(&state, &user).await?))
}

/// Fields left out stay as they are.
#[derive(Deserialize)]
struct SettingsUpdate {
    #[serde(default, deserialize_with = "present")]
    model: Option<Option<String>>,
    review: Option<String>,
}

async fn put_settings(
    State(state): State<AppState>,
    User(user): User,
    Json(update): Json<SettingsUpdate>,
) -> ApiResult<impl IntoResponse> {
    if let Some(model) = &update.model {
        let model = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
        db::set_user_model(&state.db, &user, model).await?;
    }
    if let Some(review) = &update.review {
        db::set_user_review(&state.db, &user, review_policy(review)?).await?;
    }
    Ok(Json(settings(&state, &user).await?))
}

async fn list_sessions(
    State(state): State<AppState>,
    User(user): User,
) -> ApiResult<impl IntoResponse> {
    let sessions: Vec<_> = db::list_sessions(&state.db, &user)
        .await?
        .iter()
        .map(|s| s.to_api())
        .collect();
    Ok(Json(sessions))
}

async fn get_session(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let session = db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

/// Repositories the user's GitHub App can act on; none without one.
#[derive(Deserialize)]
struct AddRepo {
    repo: String,
}

#[derive(Deserialize)]
struct SetReview {
    review: String,
}

/// Sets whether a session's proposals are reviewed by another model first (ADR 24).
async fn set_review(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    Json(body): Json<SetReview>,
) -> ApiResult<impl IntoResponse> {
    db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    db::set_session_review(&state.db, id, review_policy(&body.review)?).await?;
    state.hub.broadcast_session(id).await;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

/// Gives a session write access to another repository and tells its agent.
async fn add_repo(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    Json(body): Json<AddRepo>,
) -> ApiResult<impl IntoResponse> {
    let session = db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    let wanted = body.repo.trim();
    let repo = accessible_repos(&state, &user)
        .await
        .into_iter()
        .find(|r| r.eq_ignore_ascii_case(wanted))
        .ok_or_else(|| bad_request(format!("{wanted} isn't accessible to your GitHub App")))?;
    if proposals::grant_repo(&state.db, &session.context(), &repo).await? {
        state.hub.broadcast_session(id).await;
        state
            .hub
            .notify(
                id,
                format!(
                    "sloppod: the user gave this session write access to {repo}. {}",
                    proposals::repo_granted(&repo, id)
                ),
            )
            .await;
    }
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

async fn accessible_repos(state: &AppState, user: &str) -> Vec<String> {
    let Ok(app) = state.credentials.github(user).await else {
        return Vec::new();
    };
    app.repos().await.unwrap_or_else(|e| {
        tracing::warn!(%user, error = %e, "listing the GitHub App's repositories");
        Vec::new()
    })
}

async fn repos(State(state): State<AppState>, User(user): User) -> impl IntoResponse {
    Json(accessible_repos(&state, &user).await)
}

fn default_title(prompt: &str) -> String {
    let line = prompt
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("New session")
        .trim();
    match line.char_indices().nth(80) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_owned(),
    }
}

/// The repository a first message is about: one named as `owner/name`, or a bare name that matches exactly
/// one accessible repository. Sessions don't need one (ADR 1).
fn derive_repo(prompt: &str, repos: &[String]) -> Option<String> {
    let text = prompt.to_lowercase();
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')))
        .map(|w| w.trim_matches(|c: char| matches!(c, '.' | '/')))
        .filter(|w| !w.is_empty())
        .collect();
    let named = |repo: &&String| {
        let repo = repo.to_lowercase();
        words.iter().any(|w| {
            w.trim_end_matches(".git") == repo
                || w.contains(&format!("/{repo}/"))
                || w.ends_with(&format!("/{repo}"))
        })
    };
    let full: Vec<&String> = repos.iter().filter(named).collect();
    if let [repo] = full.as_slice() {
        return Some((*repo).clone());
    }
    let bare: Vec<&String> = repos
        .iter()
        .filter(|repo| {
            let name = repo.rsplit('/').next().unwrap_or_default().to_lowercase();
            words.iter().any(|w| w.trim_end_matches("'s") == name)
        })
        .collect();
    match bare.as_slice() {
        [repo] => Some((*repo).clone()),
        _ => None,
    }
}

async fn create_session(
    State(state): State<AppState>,
    User(user): User,
    Json(new): Json<NewSession>,
) -> ApiResult<impl IntoResponse> {
    let prompt = new.prompt.trim();
    if prompt.is_empty() {
        return Err(bad_request("a session starts from a message"));
    }
    let agent = new.agent.as_deref().unwrap_or("claude-code");
    let Some(adapter) = state.agents.get(agent) else {
        return Err(bad_request(format!("there's no agent called {agent}")));
    };
    let (kind, account) = adapter.credential();
    if state.credentials.secret(&user, kind).await?.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("add {account} in settings before starting a session"),
        ));
    }
    let repos = accessible_repos(&state, &user).await;
    let repo = match &new.repo {
        Some(wanted) => Some(
            repos
                .iter()
                .find(|r| r.eq_ignore_ascii_case(wanted.trim()))
                .cloned()
                .ok_or_else(|| {
                    bad_request(format!("{wanted} isn't accessible to your GitHub App"))
                })?,
        ),
        None => derive_repo(prompt, &repos),
    };

    let id = Uuid::new_v4();
    let mut grants = vec![
        Grant::new("model", json!({})),
        Grant::new("github", json!({})),
        Grant::new("packages", json!({})),
    ];
    if state.credentials.secret(&user, CHATGPT).await?.is_some() {
        grants.push(Grant::new("codex", json!({})));
    }
    if let Some(repo) = &repo {
        let branch = proposals::session_branch(id);
        grants.push(Grant::new(
            "repo",
            json!({"repo": repo, "access": "rw", "branch": branch}),
        ));
    }
    let model = db::user_model(&state.db, &user).await?;
    let review = db::user_review(&state.db, &user).await?;
    let custom = new
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let title = custom
        .map(str::to_owned)
        .unwrap_or_else(|| default_title(prompt));
    db::insert_session(
        &state.db,
        db::NewSessionRow {
            id,
            owner: &user,
            title: &title,
            custom_title: custom.is_some(),
            pending_prompt: prompt,
            agent,
            kind: SessionKind::Work,
            review,
            model: model.as_deref(),
            repo: repo.as_deref(),
            grants: &grants,
        },
    )
    .await?;

    state
        .hub
        .record_user_message(id, &json!([{"type": "text", "text": prompt}]), false, None)
        .await;
    let row = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    if let Err(e) = place(&state, &row).await {
        state.hub.set_phase(id, Phase::Stopped).await;
        return Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("starting the session pod: {e}"),
        ));
    }
    Ok((StatusCode::CREATED, Json(row.to_api())))
}

/// Makes sure a session's pod and egress policy exist.
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
        .grant("memory")
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
            "memory",
            json!({"namespace": namespace, "access": "rw", "key": key.name}),
        );
        db::add_grant(&state.db, session.id, &grant).await?;
    }
    Ok(())
}

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
            pod_labels: BTreeMap::from([(SESSION_LABEL.to_owned(), id.clone())]),
            intercepted_hosts: state.policies.hosts(),
            direct_hosts: if context.has("packages") {
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

async fn delete_session(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let session = db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    // Marked first, so nothing wakes it between parking and marking.
    db::mark_deleted(&state.db, id).await?;
    if let Err(e) = park_ending_turn(&state, &session).await {
        db::restore(&state.db, id).await?;
        return Err(e);
    }
    reviews::abandon(&state, id).await?;
    state.hub.broadcast_session(id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// How long a deleted session can be restored before it's purged for good.
pub const RESTORABLE_FOR: chrono::Duration = chrono::Duration::days(7);

async fn list_deleted_sessions(
    State(state): State<AppState>,
    User(user): User,
) -> ApiResult<impl IntoResponse> {
    let sessions: Vec<_> = db::deleted_sessions(&state.db, &user)
        .await?
        .iter()
        .map(db::SessionRow::to_api)
        .collect();
    Ok(Json(sessions))
}

/// Brings a deleted session back, parked; waking it is a separate step.
async fn restore_session(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let session = db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    if !db::restore(&state.db, session.id).await? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "it isn't deleted, or it's being purged".into(),
        ));
    }
    state.hub.broadcast_session(id).await;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

/// Parks a session for deletion, cancelling its turn first if it's in one.
async fn park_ending_turn(state: &AppState, session: &db::SessionRow) -> ApiResult<()> {
    for attempt in 0..30 {
        match park(state, session).await {
            Ok(()) => return Ok(()),
            Err(ParkError::InTurn) => {
                if attempt == 0 {
                    state.hub.cancel(session.id).await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            Err(e) => return Err(park_error(e)),
        }
    }
    Err(ApiError(
        StatusCode::CONFLICT,
        "the session's turn didn't stop; try again".into(),
    ))
}

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

async fn park_session(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let session = db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    park(&state, &session).await.map_err(park_error)?;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

async fn wake_session(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let session = db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    if session.deleted_at.is_some() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "the session is deleted; restore it first".into(),
        ));
    }
    wake(&state, &session).await?;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

/// Parks a session (ADR 5): its pod ends, and its volume, transcript, branch, grants and keys stay.
pub async fn park(state: &AppState, session: &db::SessionRow) -> Result<(), ParkError> {
    let id = session.id.to_string();
    state
        .hub
        .park(session.id, || async {
            state
                .placement
                .suspend(&id)
                .await
                .map_err(anyhow::Error::from)
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
    if session.context().has("memory")
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

async fn list_credentials(
    State(state): State<AppState>,
    User(user): User,
) -> ApiResult<impl IntoResponse> {
    let credentials: Vec<_> = state
        .credentials
        .list(&user)
        .await?
        .iter()
        .map(|c| c.to_api())
        .collect();
    Ok(Json(credentials))
}

async fn create_credential(
    State(state): State<AppState>,
    User(user): User,
    Json(new): Json<NewCredential>,
) -> ApiResult<impl IntoResponse> {
    match state.credentials.create(&user, new).await? {
        Ok(row) => Ok((StatusCode::CREATED, Json(row.to_api()))),
        Err(invalid) => Err(bad_request(invalid.0)),
    }
}

async fn list_standing_approvals(
    State(state): State<AppState>,
    User(user): User,
) -> ApiResult<impl IntoResponse> {
    let rows = db::standing_approvals(&state.db, &user).await?;
    Ok(Json(rows.iter().map(|r| r.to_api()).collect::<Vec<_>>()))
}

async fn create_standing_approval(
    State(state): State<AppState>,
    User(user): User,
    Json(new): Json<NewStandingApproval>,
) -> ApiResult<impl IntoResponse> {
    if state.proposals.get(&new.kind).is_none() {
        return Err(bad_request(format!("unknown proposal kind {}", new.kind)));
    }
    let target = new
        .target
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    if target.is_some_and(|t| t.split('/').count() != 2) {
        return Err(bad_request("give the target repository as owner/name"));
    }
    // Each session's autonomy is its own call, so a standing approval covers one session.
    let Some(session) = new.session_id else {
        return Err(bad_request(
            "a standing approval is for one session; give its session_id",
        ));
    };
    if db::owned_session(&state.db, session, &user)
        .await?
        .is_none()
    {
        return Err(bad_request("unknown session"));
    }
    // Only edits that make sense for every matching proposal; per-proposal edits stay with a human.
    let edits = new.edits.unwrap_or_else(|| json!({}));
    let Some(fields) = edits.as_object() else {
        return Err(bad_request("edits must be an object"));
    };
    if fields
        .iter()
        .any(|(k, v)| k != "auto_merge" || !v.is_boolean())
    {
        return Err(bad_request(
            "the only edit a standing approval can make is auto_merge: true or false",
        ));
    }
    let expires_at = match new.expires_at.as_deref() {
        Some(at) => Some(
            chrono::DateTime::parse_from_rfc3339(at)
                .map_err(|_| bad_request("expires_at must be an RFC 3339 time"))?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };
    let row = db::insert_standing_approval(
        &state.db,
        &user,
        &new.kind,
        target,
        new.session_id,
        &edits,
        expires_at,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(row.to_api())))
}

async fn revoke_standing_approval(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    if db::revoke_standing_approval(&state.db, &user, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

async fn delete_credential(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    if state.credentials.delete(&user, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

async fn default_credential(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let row = state
        .credentials
        .set_default(&user, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(row.to_api()))
}

#[derive(Deserialize)]
struct ProposalFilter {
    session: Option<Uuid>,
    status: Option<String>,
}

async fn list_proposals(
    State(state): State<AppState>,
    User(user): User,
    Query(filter): Query<ProposalFilter>,
) -> ApiResult<impl IntoResponse> {
    let status = filter
        .status
        .as_deref()
        .map(str::parse::<ProposalStatus>)
        .transpose()
        .map_err(bad_request)?;
    let proposals: Vec<_> = db::list_proposals(&state.db, &user, filter.session, status)
        .await?
        .iter()
        .map(|p| p.to_api())
        .collect();
    Ok(Json(proposals))
}

async fn get_proposal(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        proposals::detail(&state, id, &user)
            .await?
            .ok_or_else(not_found)?,
    ))
}

async fn list_reviews(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    proposals::owned_proposal(&state, id, &user)
        .await?
        .ok_or_else(not_found)?;
    let reviews: Vec<_> = db::reviews(&state.db, id)
        .await?
        .iter()
        .map(db::ReviewRow::to_api)
        .collect();
    Ok(Json(reviews))
}

async fn approve(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    body: Option<Json<serde_json::Value>>,
) -> ApiResult<impl IntoResponse> {
    let verdict = Verdict::Approve {
        edits: body.map(|Json(edits)| edits).unwrap_or_default(),
    };
    match proposals::decide(&state, id, verdict, &user, &user).await {
        Ok(Some(proposal)) => Ok(Json(proposal)),
        Ok(None) => Err(not_found()),
        Err(e) => Err(ApiError(StatusCode::CONFLICT, e.to_string())),
    }
}

async fn reject(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    body: Option<Json<Decision>>,
) -> ApiResult<impl IntoResponse> {
    let decision = body.map(|Json(d)| d).unwrap_or_default();
    match proposals::decide(
        &state,
        id,
        Verdict::Reject {
            reason: decision.reason,
        },
        &user,
        &user,
    )
    .await
    {
        Ok(Some(proposal)) => Ok(Json(proposal)),
        Ok(None) => Err(not_found()),
        Err(e) => Err(ApiError(StatusCode::CONFLICT, e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parking_conflicts_are_409s() {
        assert_eq!(park_error(ParkError::InTurn).0, StatusCode::CONFLICT);
        assert_eq!(park_error(ParkError::Stopped).0, StatusCode::CONFLICT);
        assert_eq!(
            park_error(ParkError::Failed(anyhow::anyhow!("no"))).0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    fn repos() -> Vec<String> {
        [
            "bo0tzz/sloppod",
            "bo0tzz/memini",
            "immich-app/immich",
            "bo0tzz/immich",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn lists_offered_models_flat() {
        let state = json!({"configOptions": [
            {"id": "mode", "options": [{"value": "auto", "name": "Auto"}]},
            {"id": "model", "options": [
                {"value": "default", "name": "Default"},
                {"group": "more", "name": "More", "options": [{"value": "claude-haiku-5-5", "name": "Haiku"}]},
            ]},
        ]});
        assert_eq!(
            offered_models(&state),
            vec![
                json!({"value": "default", "name": "Default"}),
                json!({"value": "claude-haiku-5-5", "name": "Haiku"}),
            ]
        );
        assert!(offered_models(&json!({})).is_empty());
    }

    #[test]
    fn derives_the_repository_from_the_first_message() {
        let repos = repos();
        let derive = |p: &str| derive_repo(p, &repos);
        assert_eq!(
            derive("In sloppod, make the UI start sessions from a message."),
            Some("bo0tzz/sloppod".into())
        );
        assert_eq!(
            derive("Fix the flaky test in immich-app/immich."),
            Some("immich-app/immich".into())
        );
        assert_eq!(
            derive("see https://github.com/bo0tzz/memini/pull/3"),
            Some("bo0tzz/memini".into())
        );
        // Two repositories are called immich, so the bare name is ambiguous.
        assert_eq!(derive("Look at immich's release process"), None);
        assert_eq!(derive("What's the weather like?"), None);
        assert_eq!(derive("Explain memoization to me"), None);
    }
}
