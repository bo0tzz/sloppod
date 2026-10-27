//! REST API (`docs/api.md`).

use std::collections::BTreeMap;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde_json::json;
use sloppod_contracts::{EgressSpec, Grant, PlacementSpec};
use sloppod_protocol::api::{Decision, NewCredential, NewSession, Phase};
use uuid::Uuid;

use crate::credentials::{CLAUDE, MEMINI};
use crate::memini::MeminiAdmin;
use crate::placement::{TOKEN_DIR, WORKDIR};
use crate::pods::SESSION_LABEL;
use crate::policies::SESSION_KEY;
use crate::proposals::{self, Verdict};
use crate::users::User;
use crate::{AppState, db};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/me", get(me))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/sessions/{id}/repos", post(add_repo))
        .route("/repos", get(repos))
        .route(
            "/credentials",
            get(list_credentials).post(create_credential),
        )
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/default", post(default_credential))
        .route("/proposals", get(list_proposals))
        .route("/proposals/{id}", get(get_proposal))
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
    Ok(json!({"model": model, "models": models}))
}

async fn get_settings(
    State(state): State<AppState>,
    User(user): User,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(settings(&state, &user).await?))
}

#[derive(Deserialize)]
struct SettingsUpdate {
    model: Option<String>,
}

async fn put_settings(
    State(state): State<AppState>,
    User(user): User,
    Json(update): Json<SettingsUpdate>,
) -> ApiResult<impl IntoResponse> {
    let model = update
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    db::set_user_model(&state.db, &user, model).await?;
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
    if state.credentials.secret(&user, CLAUDE).await?.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "add a Claude account in settings before starting a session".into(),
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
    if let Some(repo) = &repo {
        let branch = proposals::session_branch(id);
        grants.push(Grant::new(
            "repo",
            json!({"repo": repo, "access": "rw", "branch": branch}),
        ));
    }
    let model = db::user_model(&state.db, &user).await?;
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
            model: model.as_deref(),
            repo: repo.as_deref(),
            grants: &grants,
        },
    )
    .await?;

    if let Some((admin, memini)) = state.credentials.memini(&user).await? {
        let home = memini
            .config("home")
            .map(str::to_owned)
            .unwrap_or_else(|| format!("personal/{user}"));
        let namespace = state.memory_namespace(repo.as_deref(), &home);
        match admin
            .create_session_key(&id.to_string(), memini.config("home"), &namespace, false)
            .await
        {
            Ok(key) => {
                state
                    .credentials
                    .put_session_secret(id, SESSION_KEY, &key.secret)
                    .await?;
                let grant = Grant::new(
                    "memory",
                    json!({"namespace": namespace, "access": "rw", "key": key.name}),
                );
                db::add_grant(&state.db, id, &grant).await?;
            }
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "minting the session's memini key; no memory")
            }
        }
    }

    state
        .hub
        .record_user_message(id, &json!([{"type": "text", "text": prompt}]), false)
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
pub async fn place(state: &AppState, session: &db::SessionRow) -> anyhow::Result<()> {
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
    state.hub.stop(id).await;
    state.placement.delete(&id.to_string()).await?;
    state.enforcer.remove(&id.to_string()).await?;
    if session.context().has("memory")
        && let Some((admin, _)) = state.credentials.memini(&user).await?
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
    Ok(StatusCode::NO_CONTENT)
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
    let kind = new.kind.clone();
    match state.credentials.create(&user, new).await? {
        Ok(row) => {
            if kind == MEMINI
                && let Err(e) = state.install_listener().await
            {
                tracing::error!(error = %e, "updating the egress listener for a new memini host");
            }
            Ok((StatusCode::CREATED, Json(row.to_api())))
        }
        Err(invalid) => Err(bad_request(invalid.0)),
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
    let proposals: Vec<_> =
        db::list_proposals(&state.db, &user, filter.session, filter.status.as_deref())
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

async fn approve(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    body: Option<Json<Decision>>,
) -> ApiResult<impl IntoResponse> {
    let decision = body.map(|Json(d)| d).unwrap_or_default();
    let verdict = Verdict::Approve {
        title: decision.title,
        body: decision.body,
    };
    match proposals::decide(&state, id, verdict, &user).await {
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
