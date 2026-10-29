//! The owner's REST API for work graphs (`docs/api.md`, "Work"). Owners see and change only their own (ADR 23).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::rules::{LinkStatus, State as ItemState};
use super::{Actor, ItemEdit, ItemMove, NewItem, NewLink, WorkError};
use crate::AppState;
use crate::api::{ApiError, ApiResult, not_found};
use crate::users::User;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/work/graphs", get(list_graphs).post(create_graph))
        .route(
            "/work/graphs/{id}",
            get(get_graph).patch(rename_graph).delete(delete_graph),
        )
        .route("/work/graphs/{id}/frontier", get(frontier))
        .route("/work/graphs/{id}/history", get(graph_history))
        .route("/work/graphs/{id}/items", post(create_item))
        .route("/work/items/{id}", get(get_item).patch(edit_item))
        .route("/work/items/{id}/state", post(move_state))
        .route("/work/items/{id}/move", post(move_item))
        .route("/work/items/{id}/blocks", post(add_block))
        .route("/work/items/{id}/blocks/{other}", delete(remove_block))
        .route("/work/items/{id}/links", post(add_link))
        .route("/work/items/{id}/history", get(item_history))
        .route(
            "/work/links/{id}",
            patch(set_link_status).delete(remove_link),
        )
        .route("/work/events/{id}/undo", post(undo))
}

fn work_error(e: WorkError) -> ApiError {
    match e {
        WorkError::NotFound => not_found(),
        WorkError::Invalid(message) => ApiError(StatusCode::BAD_REQUEST, message),
        WorkError::Conflict(message) => ApiError(StatusCode::CONFLICT, message),
        WorkError::Db(e) => e.into(),
    }
}

fn owner(user: &str) -> Actor<'_> {
    Actor {
        user,
        session: None,
    }
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

async fn list_graphs(
    State(state): State<AppState>,
    User(user): User,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(super::list_graphs(&state.db, &user).await?))
}

async fn create_graph(
    State(state): State<AppState>,
    User(user): User,
    Json(new): Json<Named>,
) -> ApiResult<impl IntoResponse> {
    let graph = super::create_graph(&state.db, &user, &new.name)
        .await
        .map_err(work_error)?;
    Ok((StatusCode::CREATED, Json(graph)))
}

async fn get_graph(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let graph = super::graph(&state.db, &user, id)
        .await?
        .ok_or_else(not_found)?;
    let items = super::graph_items(&state.db, &user, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(json!({
        "id": graph.id,
        "name": graph.name,
        "created_at": graph.created_at,
        "items": items,
    })))
}

async fn rename_graph(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    Json(new): Json<Named>,
) -> ApiResult<impl IntoResponse> {
    let graph = super::rename_graph(&state.db, owner(&user), id, &new.name)
        .await
        .map_err(work_error)?;
    Ok(Json(graph))
}

async fn delete_graph(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    if super::delete_graph(&state.db, &user, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

async fn frontier(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let ready = super::frontier(&state.db, &user, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(ready))
}

async fn graph_history(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let events = super::graph_history(&state.db, &user, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(events))
}

async fn create_item(
    State(state): State<AppState>,
    User(user): User,
    Path(graph): Path<Uuid>,
    Json(new): Json<NewItem>,
) -> ApiResult<impl IntoResponse> {
    let item = super::create_item(&state.db, owner(&user), graph, &new)
        .await
        .map_err(work_error)?;
    Ok((StatusCode::CREATED, Json(item)))
}

async fn get_item(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        super::item(&state.db, &user, id)
            .await?
            .ok_or_else(not_found)?,
    ))
}

async fn edit_item(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    Json(edit): Json<ItemEdit>,
) -> ApiResult<impl IntoResponse> {
    let item = super::edit_item(&state.db, owner(&user), id, &edit)
        .await
        .map_err(work_error)?;
    Ok(Json(item))
}

#[derive(Deserialize)]
struct StateChange {
    state: ItemState,
}

async fn move_state(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    Json(change): Json<StateChange>,
) -> ApiResult<impl IntoResponse> {
    let item = super::move_state(&state.db, owner(&user), id, change.state)
        .await
        .map_err(work_error)?;
    Ok(Json(item))
}

async fn move_item(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    Json(to): Json<ItemMove>,
) -> ApiResult<impl IntoResponse> {
    let item = super::move_item(&state.db, owner(&user), id, &to)
        .await
        .map_err(work_error)?;
    Ok(Json(item))
}

#[derive(Deserialize)]
struct Blocks {
    item: Uuid,
}

async fn add_block(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    Json(blocks): Json<Blocks>,
) -> ApiResult<impl IntoResponse> {
    let item = super::add_block(&state.db, owner(&user), id, blocks.item)
        .await
        .map_err(work_error)?;
    Ok(Json(item))
}

async fn remove_block(
    State(state): State<AppState>,
    User(user): User,
    Path((id, other)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    let item = super::remove_block(&state.db, owner(&user), id, other)
        .await
        .map_err(work_error)?;
    Ok(Json(item))
}

async fn add_link(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    Json(new): Json<NewLink>,
) -> ApiResult<impl IntoResponse> {
    let link = super::add_link(&state.db, owner(&user), id, &new)
        .await
        .map_err(work_error)?;
    Ok((StatusCode::CREATED, Json(link)))
}

#[derive(Deserialize)]
struct StatusChange {
    status: LinkStatus,
}

async fn set_link_status(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
    Json(change): Json<StatusChange>,
) -> ApiResult<impl IntoResponse> {
    let link = super::set_link_status(&state.db, owner(&user), id, change.status)
        .await
        .map_err(work_error)?;
    Ok(Json(link))
}

async fn remove_link(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    super::remove_link(&state.db, owner(&user), id)
        .await
        .map_err(work_error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn item_history(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let events = super::item_history(&state.db, &user, id)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(events))
}

async fn undo(
    State(state): State<AppState>,
    User(user): User,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let event = super::undo(&state.db, owner(&user), id)
        .await
        .map_err(work_error)?;
    Ok((StatusCode::CREATED, Json(event)))
}
