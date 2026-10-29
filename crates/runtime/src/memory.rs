//! Sessions reach memini through the runtime, on the sessions port, so memini can live at any address the runtime
//! can reach, including one inside the cluster. The memini policy decides each request (ADR 8).

use std::collections::BTreeMap;
use std::net::SocketAddr;

use anyhow::Context;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use sloppod_contracts::{Decision, HttpRequest, RequestPolicy};

use crate::credentials::MEMINI;
use crate::{AppState, db};

/// Where sessions find memini on the runtime's sessions port.
pub const PREFIX: &str = "/memini";

const MAX_BODY: usize = 8 << 20;

pub async fn proxy(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    match forward(&state, peer, request).await {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!(%peer, error = %format!("{e:#}"), "proxying to memini");
            (
                StatusCode::BAD_GATEWAY,
                format!("sloppod: memini isn't reachable: {e:#}\n"),
            )
                .into_response()
        }
    }
}

async fn forward(state: &AppState, peer: SocketAddr, request: Request) -> anyhow::Result<Response> {
    let Some(session) = crate::identity::calling_session(state, request.headers(), peer.ip()).await
    else {
        return Ok((
            StatusCode::FORBIDDEN,
            "sloppod: memory is only for session pods\n",
        )
            .into_response());
    };
    let session = db::get_session(&state.db, session)
        .await?
        .context("unknown session")?;
    if let Some(why) = session.not_acting() {
        return Ok((
            StatusCode::FORBIDDEN,
            format!("sloppod: this session is {why}, so it has no memory\n"),
        )
            .into_response());
    }
    // Only its URL is needed here; the admin key stays sealed.
    let Some(credential) = state
        .credentials
        .default_row(&session.owner, MEMINI)
        .await?
    else {
        return Ok((StatusCode::NOT_FOUND, "sloppod: no memini is configured\n").into_response());
    };
    let base = credential
        .config("url")
        .context("the memini credential has no URL")?
        .trim_end_matches('/')
        .to_owned();

    let (parts, body) = request.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let path = match path_and_query.strip_prefix(PREFIX) {
        Some(rest) if rest.starts_with('/') => rest.to_owned(),
        Some(rest) => format!("/{rest}"),
        None => path_and_query.to_owned(),
    };
    let Ok(path) = crate::proxy::canonical_path(&path) else {
        return Ok((StatusCode::BAD_REQUEST, "sloppod: invalid memini path\n").into_response());
    };
    let body = axum::body::to_bytes(body, MAX_BODY)
        .await
        .context("reading the request body")?;
    let headers: BTreeMap<String, String> = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect();
    let decision = state
        .memory
        .decide(
            &session.context(),
            &HttpRequest {
                method: parts.method.to_string(),
                host: reqwest::Url::parse(&base)?
                    .host_str()
                    .unwrap_or_default()
                    .to_owned(),
                path: path.clone(),
                headers,
                body: body.to_vec(),
                body_truncated: false,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.0))?;
    let (set_headers, remove_headers) = match decision {
        Decision::Allow {
            set_headers,
            remove_headers,
        } => sloppod_contracts::header_ops(set_headers, remove_headers),
        Decision::Deny {
            status,
            message,
            content_type,
        } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
            let content_type = content_type.unwrap_or_else(|| "text/plain; charset=utf-8".into());
            return Ok((status, [(header::CONTENT_TYPE, content_type)], message).into_response());
        }
    };

    let url = format!("{base}{path}");
    crate::proxy::forward(
        &crate::proxy::HTTP,
        crate::proxy::Forward {
            method: parts.method,
            headers: &parts.headers,
            url: &url,
            body,
            set: set_headers,
            remove: remove_headers,
            user_header: &state.config.user_header,
        },
    )
    .await
}
