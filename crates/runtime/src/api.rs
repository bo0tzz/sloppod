//! REST API (`docs/api.md`).

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use serde::Deserialize;
use serde_json::json;
use sloppod_contracts::{Grant, find_repo};
use sloppod_protocol::api::{
    Decision, NewCredential, NewSession, NewStandingApproval, ProposalStatus, ReviewPolicy,
};
use uuid::Uuid;

use crate::proposals::{self, DecideError, Verdict};
use crate::session::ParkError;
use crate::users::User;
use crate::{AppState, db, lifecycle, quota, reviews, subsessions};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/me", get(me))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/suggest", post(suggest_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/repos", post(add_repo))
        .route("/sessions/{id}/grants", post(add_grant))
        .route(
            "/sessions/{id}/grants/{grant}",
            axum::routing::patch(change_grant).delete(revoke_grant),
        )
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
        .route(
            "/capabilities",
            get(list_definitions).post(create_definition),
        )
        .route("/capabilities/{id}", delete(delete_definition))
        .route("/credentials/{id}/default", post(default_credential))
        .route("/quotas", get(list_quotas))
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

/// Edits that don't apply are the request's fault, a decided proposal is a conflict, and the rest is ours.
fn decide_error(e: DecideError) -> ApiError {
    match e {
        DecideError::Invalid(message) => bad_request(message),
        DecideError::Decided => ApiError(StatusCode::CONFLICT, e.to_string()),
        DecideError::Failed(e) => e.into(),
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

async fn me(State(state): State<AppState>, User(user): User) -> impl IntoResponse {
    Json(json!({"user": user, "version": state.config.version, "build": state.config.build}))
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
    let timezone = db::user_timezone(&state.db, user).await?;
    Ok(json!({"model": model, "models": models, "review": review, "timezone": timezone}))
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
    timezone: Option<String>,
}

async fn put_settings(
    State(state): State<AppState>,
    User(user): User,
    Json(update): Json<SettingsUpdate>,
) -> ApiResult<impl IntoResponse> {
    // Checked before anything is written, so a bad field doesn't leave the others half applied.
    let review = update.review.as_deref().map(review_policy).transpose()?;
    let timezone = update
        .timezone
        .as_deref()
        .map(|name| {
            name.trim()
                .parse::<chrono_tz::Tz>()
                .map_err(|_| bad_request(format!("{name} isn't a timezone, e.g. Europe/Amsterdam")))
        })
        .transpose()?;
    if let Some(model) = &update.model {
        let model = model.as_deref().map(str::trim).filter(|m| !m.is_empty());
        db::set_user_model(&state.db, &user, model).await?;
    }
    if let Some(review) = review {
        db::set_user_review(&state.db, &user, review).await?;
    }
    if let Some(timezone) = timezone {
        db::set_user_timezone(&state.db, &user, timezone.name()).await?;
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
    /// One of the owner's GitHub tokens to act with on it (ADR 29), by id; none for the GitHub App.
    #[serde(default)]
    credential: Option<String>,
}

#[derive(Deserialize)]
struct NewGrant {
    capability: String,
    /// RFC 3339; none: it doesn't run out.
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize)]
struct GrantChange {
    /// A new expiry: a time, or `null` for none. Absent leaves it.
    #[serde(default, deserialize_with = "present")]
    expires_at: Option<Option<chrono::DateTime<chrono::Utc>>>,
    /// Narrows the grant, e.g. a repository the session can push to becomes read only.
    #[serde(default)]
    narrow: bool,
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
    if let Some(credential) = body.credential.as_deref() {
        return add_repo_as(&state, &user, id, wanted, credential).await;
    }
    let accessible = accessible_repos(&state, &user).await;
    let repo = find_repo(&accessible, wanted)
        .cloned()
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

/// Has a session act on `repo` with one of the owner's GitHub tokens (ADR 29): write access to it if the session
/// lacks it, and its grant switched to the token. Only the owner does this; the token has to reach the repository.
async fn add_repo_as(
    state: &AppState,
    user: &str,
    id: Uuid,
    wanted: &str,
    credential: &str,
) -> ApiResult<Json<sloppod_protocol::api::Session>> {
    let row = state
        .credentials
        .github_user(user, credential)
        .await?
        .ok_or_else(|| bad_request("that isn't one of your GitHub tokens"))?;
    let repo = crate::credentials::token_reaches(&row, wanted)
        .ok_or_else(|| bad_request(format!("the token {} doesn't reach {wanted}", row.name)))?;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    proposals::grant_repo_as(&state.db, &session.context(), &repo, &row).await?;
    state.hub.broadcast_session(id).await;
    state
        .hub
        .notify(
            id,
            format!(
                "sloppod: the user gave this session write access to {repo}. {}",
                proposals::repo_granted_as(&repo, &row, id)
            ),
        )
        .await;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

/// Grants a session a capability it doesn't hold (ADR 2), from its owner. Repositories go through `add_repo`.
async fn add_grant(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    Json(body): Json<NewGrant>,
) -> ApiResult<impl IntoResponse> {
    let session = db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    let capability = body.capability.trim();
    // A built-in, or one of the capabilities the owner defined.
    let defined = crate::definitions::named(&state.db, &user, capability).await?;
    if !crate::grants::grantable(capability) && defined.is_none() {
        return Err(bad_request(format!(
            "{capability} can't be granted here; repositories are added with POST /sessions/{{id}}/repos"
        )));
    }
    if session.context().has(capability) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("the session already has {capability}"),
        ));
    }
    if body.expires_at.is_some_and(|at| at <= chrono::Utc::now()) {
        return Err(bad_request("expires_at is in the past"));
    }
    let grant = match &defined {
        Some(definition) => crate::definitions::grant(definition),
        None => Grant::new(capability, json!({})),
    };
    db::grant(&state.db, id, &grant, body.expires_at).await?;
    // An agent gets its MCP servers when it starts, so a server granted now reaches it then.
    let later = if crate::definitions::serves(&grant) {
        " Its MCP server reaches you when your agent next starts, e.g. after the session is parked and woken."
    } else {
        ""
    };
    let what = format!(
        "the user granted this session {capability}: {}{later}",
        crate::grants::describe(&grant)
    );
    lifecycle::grants_changed(&state, id, &what).await;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok((StatusCode::CREATED, Json(session.to_api())))
}

/// Extends, shortens or clears a grant's expiry, or narrows it.
async fn change_grant(
    State(state): State<AppState>,
    User(user): User,
    Path((id, grant_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<GrantChange>,
) -> ApiResult<impl IntoResponse> {
    db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    let grant = db::session_grant(&state.db, id, grant_id)
        .await?
        .ok_or_else(not_found)?;
    // Everything is checked before anything is written, and then written together.
    if !body.narrow && body.expires_at.is_none() {
        return Err(bad_request("give expires_at or narrow"));
    }
    if body
        .expires_at
        .flatten()
        .is_some_and(|at| at <= chrono::Utc::now())
    {
        return Err(bad_request(
            "expires_at is in the past; revoke the grant instead",
        ));
    }
    let narrower = match body.narrow {
        true => Some(
            crate::grants::narrowed(&grant)
                .ok_or_else(|| bad_request("this grant can't be narrowed"))?,
        ),
        false => None,
    };
    db::change_grant(&state.db, id, grant_id, narrower.as_ref(), body.expires_at).await?;
    let mut changes = Vec::new();
    if let Some(params) = narrower {
        let narrowed = Grant {
            params,
            ..grant.clone()
        };
        changes.push(format!(
            "narrowed to: {}",
            crate::grants::describe(&narrowed)
        ));
    }
    if let Some(expires_at) = body.expires_at {
        changes.push(match expires_at {
            Some(at) => format!("now runs out at {}", at.to_rfc3339()),
            None => "no longer runs out".to_owned(),
        });
    }
    let what = format!(
        "the user changed this session's grant {} ({}): {}.",
        grant.capability,
        crate::grants::describe(&grant),
        changes.join("; ")
    );
    lifecycle::grants_changed(&state, id, &what).await;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

/// Revokes a grant; it stops counting at once, at the authorizer.
async fn revoke_grant(
    State(state): State<AppState>,
    User(user): User,
    Path((id, grant_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    let grant = db::revoke_grant(&state.db, id, grant_id)
        .await?
        .ok_or_else(not_found)?;
    let what = format!(
        "the user revoked this session's grant {}: {}",
        grant.capability,
        crate::grants::describe(&grant)
    );
    lifecycle::grants_changed(&state, id, &what).await;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

pub(crate) async fn accessible_repos(state: &AppState, user: &str) -> Vec<String> {
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

pub(crate) fn default_title(prompt: &str) -> String {
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
pub(crate) fn derive_repo(prompt: &str, repos: &[String]) -> Option<String> {
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
    let needed = adapter.credential();
    let (kind, account) = (needed.kind.as_str(), needed.name.as_str());
    if !state.credentials.has_default(&user, kind).await? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("add {account} in settings before starting a session"),
        ));
    }
    let id = Uuid::new_v4();
    let (grants, repo) = match &new.grants {
        // The starting set as grants: what the user chose, checked.
        Some(wanted) => {
            if new.repo.is_some() || new.capabilities.is_some() {
                return Err(bad_request(
                    "give grants, or repo and capabilities, not both",
                ));
            }
            crate::starting::from_grants(&state, &user, wanted, id)
                .await?
                .map_err(bad_request)?
        }
        None => {
            let repos = accessible_repos(&state, &user).await;
            let repo = match &new.repo {
                Some(None) => None,
                Some(Some(wanted)) => {
                    Some(find_repo(&repos, wanted.trim()).cloned().ok_or_else(|| {
                        bad_request(format!("{wanted} isn't accessible to your GitHub App"))
                    })?)
                }
                None => derive_repo(prompt, &repos),
            };
            let defaults = crate::starting::defaults(&state, &user).await?;
            let grants = crate::starting::grants(
                &defaults,
                new.capabilities.as_deref(),
                repo.as_deref(),
                id,
            )
            .map_err(bad_request)?;
            (grants, repo)
        }
    };
    let model = match new
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        Some(model) => Some(model.to_owned()),
        None => db::user_model(&state.db, &user).await?,
    };
    let review = db::user_review(&state.db, &user).await?;
    let custom = new
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let title = custom
        .map(str::to_owned)
        .unwrap_or_else(|| default_title(prompt));
    let row = lifecycle::start_session(
        &state,
        id,
        lifecycle::NewSession {
            owner: &user,
            prompt,
            title: &title,
            custom_title: custom.is_some(),
            agent,
            model: model.as_deref(),
            review,
            repo: repo.as_deref(),
            grants: &grants,
            parent: None,
        },
    )
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok((StatusCode::CREATED, Json(row.to_api())))
}

#[derive(Deserialize)]
struct Draft {
    prompt: String,
}

/// The starting set suggested for a first message as it's written (#232): `{title, repo, capabilities, source}`.
async fn suggest_session(
    State(state): State<AppState>,
    User(user): User,
    Json(draft): Json<Draft>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        crate::starting::suggest(&state, &user, &draft.prompt).await?,
    ))
}

async fn delete_session(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let session = db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    // Its sub-sessions stop with it (ADR 27), at any depth, including any restored below one already deleted. Spawning
    // waits while the root is marked and the tree listed, and refuses below a deleted session after.
    let mut pending = {
        let _spawning = subsessions::SPAWNING.lock().await;
        db::mark_deleted(&state.db, id).await?;
        db::live_descendants(&state.db, id).await?
    };
    pending.push(session);
    while let Some(session) = pending.pop() {
        let id = session.id;
        // Marked first, so nothing wakes it between parking and marking.
        db::mark_deleted(&state.db, id).await?;
        if let Err(e) = park_ending_turn(&state, &session).await {
            db::restore(&state.db, id).await?;
            return Err(e);
        }
        reviews::abandon(&state, id).await?;
        state.hub.broadcast_session(id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

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
        match lifecycle::park(state, session).await {
            // A stopped session, e.g. one whose pod never started, has nothing to park.
            Ok(()) | Err(ParkError::Stopped) => return Ok(()),
            Err(ParkError::InTurn) => {
                if attempt == 0 {
                    state.hub.cancel(session.id, "deleting the session").await;
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

async fn park_session(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let session = db::owned_session(&state.db, id, &user)
        .await?
        .ok_or_else(not_found)?;
    lifecycle::park(&state, &session)
        .await
        .map_err(park_error)?;
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
    lifecycle::wake(&state, &session).await?;
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

/// The capabilities the user defined themselves (ADR 2): so far, MCP servers.
async fn list_definitions(
    State(state): State<AppState>,
    User(user): User,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(crate::definitions::list(&state.db, &user).await?))
}

async fn create_definition(
    State(state): State<AppState>,
    User(user): User,
    Json(new): Json<crate::definitions::NewDefinition>,
) -> ApiResult<impl IntoResponse> {
    if let Some(why) = crate::definitions::invalid(&new) {
        return Err(bad_request(why));
    }
    if let Some(credential) = new.credential_id
        && state.credentials.token(&user, credential).await?.is_none()
    {
        return Err(bad_request(
            "credential_id isn't one of your token credentials",
        ));
    }
    let definition = crate::definitions::create(&state.db, &user, &new)
        .await?
        .ok_or_else(|| {
            ApiError(
                StatusCode::CONFLICT,
                format!("you have a capability called {} already", new.name),
            )
        })?;
    Ok((StatusCode::CREATED, Json(definition)))
}

async fn delete_definition(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    if crate::definitions::delete(&state.db, &user, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
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

async fn list_quotas(
    State(state): State<AppState>,
    User(user): User,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        quota::list(&state.db, &user, chrono::Utc::now()).await?,
    ))
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
    let Some(kind) = state.proposals.get(&new.kind) else {
        return Err(bad_request(format!("unknown proposal kind {}", new.kind)));
    };
    if !kind.standing_approvable() {
        return Err(bad_request(format!(
            "{} proposals always wait for a person to decide them",
            new.kind
        )));
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
        Err(e) => Err(decide_error(e)),
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
        Err(e) => Err(decide_error(e)),
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

    #[test]
    fn decisions_fail_by_whose_fault_it_is() {
        assert_eq!(
            decide_error(DecideError::Invalid("bad edit".into())).0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(decide_error(DecideError::Decided).0, StatusCode::CONFLICT);
        assert_eq!(
            decide_error(DecideError::Failed(anyhow::anyhow!("db down"))).0,
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
