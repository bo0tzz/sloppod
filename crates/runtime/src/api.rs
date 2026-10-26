//! REST API (`docs/api.md`).

use std::collections::BTreeMap;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;
use sloppod_contracts::{EgressSpec, PlacementSpec};
use sloppod_protocol::api::{Decision, NewSession, Phase};
use uuid::Uuid;

use crate::placement::{TOKEN_DIR, WORKDIR};
use crate::pods::SESSION_LABEL;
use crate::proposals::{self, Verdict};
use crate::{AppState, db};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session).delete(delete_session))
        .route("/repos", get(repos))
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

type ApiResult<T> = Result<T, ApiError>;

async fn list_sessions(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    let sessions: Vec<_> = db::list_sessions(&state.db)
        .await?
        .iter()
        .map(|s| s.to_api())
        .collect();
    Ok(Json(sessions))
}

async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let session = db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(session.to_api()))
}

async fn repos(State(state): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(state.github.repos().await?))
}

fn default_title(new: &NewSession) -> String {
    let from_prompt = new
        .prompt
        .as_deref()
        .and_then(|p| p.lines().find(|l| !l.trim().is_empty()))
        .map(|l| {
            let l = l.trim();
            match l.char_indices().nth(80) {
                Some((i, _)) => format!("{}…", &l[..i]),
                None => l.to_owned(),
            }
        });
    from_prompt.unwrap_or_else(|| new.repo.clone())
}

async fn create_session(
    State(state): State<AppState>,
    Json(new): Json<NewSession>,
) -> ApiResult<impl IntoResponse> {
    let repo = new.repo.trim().to_owned();
    if repo.split('/').count() != 2 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "repo must be owner/name".into(),
        ));
    }
    if let Err(e) = state.github.token_for(&repo).await {
        return Err(ApiError(StatusCode::BAD_REQUEST, e.to_string()));
    }
    let id = Uuid::new_v4();
    let branch = format!("sloppod/{}", &id.simple().to_string()[..8]);
    let title = new
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| default_title(&new));
    let row =
        db::insert_session(&state.db, id, &title, &repo, &branch, new.prompt.as_deref()).await?;
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
            direct_hosts: state.config.direct_hosts.clone(),
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
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    db::get_session(&state.db, id)
        .await?
        .ok_or_else(not_found)?;
    state.hub.stop(id).await;
    state.placement.delete(&id.to_string()).await?;
    state.enforcer.remove(&id.to_string()).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ProposalFilter {
    session: Option<Uuid>,
    status: Option<String>,
}

async fn list_proposals(
    State(state): State<AppState>,
    Query(filter): Query<ProposalFilter>,
) -> ApiResult<impl IntoResponse> {
    let proposals: Vec<_> = db::list_proposals(&state.db, filter.session, filter.status.as_deref())
        .await?
        .iter()
        .map(|p| p.to_api())
        .collect();
    Ok(Json(proposals))
}

async fn get_proposal(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        proposals::detail(&state, id).await?.ok_or_else(not_found)?,
    ))
}

async fn approve(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<Decision>>,
) -> ApiResult<impl IntoResponse> {
    let decision = body.map(|Json(d)| d).unwrap_or_default();
    let verdict = Verdict::Approve {
        title: decision.title,
        body: decision.body,
    };
    match proposals::decide(&state, id, verdict).await {
        Ok(Some(proposal)) => Ok(Json(proposal)),
        Ok(None) => Err(not_found()),
        Err(e) => Err(ApiError(StatusCode::CONFLICT, e.to_string())),
    }
}

async fn reject(
    State(state): State<AppState>,
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
    )
    .await
    {
        Ok(Some(proposal)) => Ok(Json(proposal)),
        Ok(None) => Err(not_found()),
        Err(e) => Err(ApiError(StatusCode::CONFLICT, e.to_string())),
    }
}
