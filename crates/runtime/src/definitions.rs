//! Capabilities an owner defines themselves (ADR 2, 2026-10-29). So far that's an MCP server: the runtime serves it
//! to the sessions granted it, at `/servers/<name>`, with the definition's credential attached on the way, so the
//! token never enters the pod. Sloppod doesn't know or care what the server does. A built-in that's an MCP server
//! (`docs`) is served the same way, from its URL in `capabilities.rs`, with no credential.

use std::net::SocketAddr;

use anyhow::Context;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sloppod_contracts::Grant;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{AppState, db};

/// Where sessions reach their definitions' servers, on the sessions port.
pub const PREFIX: &str = "/servers";

/// The largest request body forwarded to a server.
const MAX_BODY: usize = 8 << 20;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Definition {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub mcp_url: String,
    pub credential_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct NewDefinition {
    pub name: String,
    pub description: String,
    pub mcp_url: String,
    #[serde(default)]
    pub credential_id: Option<Uuid>,
}

const COLUMNS: &str = "id, name, description, mcp_url, credential_id, created_at";

/// Why a definition can't be made, for a `400`.
pub fn invalid(new: &NewDefinition) -> Option<String> {
    let name = new.name.as_str();
    let well_formed = (1..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !well_formed {
        return Some(
            "a name is 1 to 32 lowercase letters, digits and dashes, starting with a letter".into(),
        );
    }
    if crate::capabilities::reserved(name) {
        return Some(format!("{name} is a built-in capability's name"));
    }
    if new.description.trim().is_empty() {
        return Some(
            "say what it's for in a description: the agent and the grants list show it".into(),
        );
    }
    match reqwest::Url::parse(&new.mcp_url) {
        Ok(url) if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() => {
            if !url.username().is_empty() || url.password().is_some() {
                return Some("put a credential on the definition rather than in its URL".into());
            }
            None
        }
        _ => Some("mcp_url is the server's http(s) URL".into()),
    }
}

pub async fn list(db: &PgPool, owner: &str) -> sqlx::Result<Vec<Definition>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {COLUMNS} from capability_definitions where owner = $1 order by name"
    )))
    .bind(owner)
    .fetch_all(db)
    .await
}

pub async fn named(db: &PgPool, owner: &str, name: &str) -> sqlx::Result<Option<Definition>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {COLUMNS} from capability_definitions where owner = $1 and name = $2"
    )))
    .bind(owner)
    .bind(name)
    .fetch_optional(db)
    .await
}

/// Makes a definition; none if the owner has one by that name already.
pub async fn create(
    db: &PgPool,
    owner: &str,
    new: &NewDefinition,
) -> sqlx::Result<Option<Definition>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "insert into capability_definitions (id, owner, name, description, mcp_url, credential_id) \
         values ($1, $2, $3, $4, $5, $6) on conflict (owner, name) do nothing returning {COLUMNS}"
    )))
    .bind(Uuid::now_v7())
    .bind(owner)
    .bind(&new.name)
    .bind(new.description.trim())
    .bind(&new.mcp_url)
    .bind(new.credential_id)
    .fetch_optional(db)
    .await
}

/// Deletes one of the owner's definitions. Grants of it stay, and stop working: the server isn't served any more.
pub async fn delete(db: &PgPool, owner: &str, id: Uuid) -> sqlx::Result<bool> {
    Ok(
        sqlx::query("delete from capability_definitions where id = $1 and owner = $2")
            .bind(id)
            .bind(owner)
            .execute(db)
            .await?
            .rows_affected()
            > 0,
    )
}

/// A grant of a definition: its name as the capability, and what it is, for the grants list and the agent.
pub fn grant(definition: &Definition) -> Grant {
    Grant::new(
        &definition.name,
        json!({"definition": definition.id, "description": definition.description}),
    )
}

/// Whether a grant is of an MCP server the runtime serves: a definition's, or a built-in that's one.
pub fn serves(grant: &Grant) -> bool {
    grant.params.get("definition").is_some()
        || crate::capabilities::mcp_url(&grant.capability).is_some()
}

/// The MCP servers a session's grants give its agent, as (name, URL under `base`).
pub fn servers(grants: &[Grant], base: &str) -> Vec<(String, String)> {
    grants
        .iter()
        .filter(|grant| serves(grant))
        .map(|grant| {
            (
                grant.capability.clone(),
                format!("{base}{PREFIX}/{}", grant.capability),
            )
        })
        .collect()
}

/// `/servers/{name}` and below: a session's request to one of its MCP servers, forwarded if the session holds a
/// grant of it: a definition's with its credential, a built-in's with none.
pub async fn proxy(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(params): Path<Vec<(String, String)>>,
    request: Request,
) -> Response {
    let name = params
        .iter()
        .find(|(key, _)| key == "name")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    match forward(&state, peer, &name, request).await {
        Ok(response) => response,
        Err(e) => {
            tracing::warn!(%peer, server = %name, error = %format!("{e:#}"), "proxying to an MCP server");
            (
                StatusCode::BAD_GATEWAY,
                format!("sloppod: the {name} server isn't reachable: {e:#}\n"),
            )
                .into_response()
        }
    }
}

fn refuse(status: StatusCode, message: String) -> anyhow::Result<Response> {
    Ok((status, message).into_response())
}

async fn forward(
    state: &AppState,
    peer: SocketAddr,
    name: &str,
    request: Request,
) -> anyhow::Result<Response> {
    let Some(session) = crate::identity::calling_session(state, request.headers(), peer.ip()).await
    else {
        return refuse(
            StatusCode::FORBIDDEN,
            "sloppod: servers are only for session pods\n".into(),
        );
    };
    let session = db::get_session(&state.db, session)
        .await?
        .context("unknown session")?;
    if let Some(why) = session.not_acting() {
        return refuse(
            StatusCode::FORBIDDEN,
            format!("sloppod: this session is {why}, so it reaches no servers\n"),
        );
    }
    let upstream =
        match upstream(&state.db, &session.owner, &session.context().grants, name).await? {
            Ok(upstream) => upstream,
            Err((status, message)) => return refuse(status, message),
        };

    let (parts, body) = request.into_parts();
    let below = parts
        .uri
        .path()
        .strip_prefix(&format!("{PREFIX}/{name}"))
        .unwrap_or("");
    let url = upstream_url(&upstream.mcp_url, below, parts.uri.query())?;
    let body = axum::body::to_bytes(body, MAX_BODY)
        .await
        .context("reading the request body")?;
    // Whatever the agent sent as credentials isn't passed on: the definition's is the only one.
    let mut set = Vec::new();
    if let Some(credential) = upstream.credential_id {
        set.push(token_header(state, &session.owner, credential).await?);
    }
    tracing::info!(session = %session.id, server = %name, method = %parts.method, "request to an MCP server");
    state
        .reach
        .forward(crate::proxy::Forward {
            method: parts.method,
            headers: &parts.headers,
            url: &url,
            body,
            set,
            remove: vec!["authorization".into(), "cookie".into()],
            user_header: &state.config.user_header,
        })
        .await
}

/// The server a session's request for `name` goes to.
#[derive(Debug, PartialEq, Eq)]
pub struct Upstream {
    pub mcp_url: String,
    pub credential_id: Option<Uuid>,
}

/// Which server a session with `grants` reaches as `name`, or why it reaches none, as a status and a message for the
/// agent: a built-in that's an MCP server from its URL with no credential, or the owner's definition of that name if
/// it's the one granted.
pub async fn upstream(
    db: &PgPool,
    owner: &str,
    grants: &[Grant],
    name: &str,
) -> sqlx::Result<Result<Upstream, (StatusCode, String)>> {
    let Some(grant) = grants
        .iter()
        .find(|grant| grant.capability == name && serves(grant))
    else {
        return Ok(Err((
            StatusCode::FORBIDDEN,
            format!(
                "sloppod: this session has no grant of {name}; the user can grant it in the session's grants\n"
            ),
        )));
    };
    if let Some(url) = crate::capabilities::mcp_url(name) {
        return Ok(Ok(Upstream {
            mcp_url: url.to_owned(),
            credential_id: None,
        }));
    }
    let Some(definition) = named(db, owner, name).await? else {
        return Ok(Err((
            StatusCode::NOT_FOUND,
            format!("sloppod: the {name} capability was deleted; tell the user\n"),
        )));
    };
    // A definition deleted and made again under its name isn't the one granted.
    if grant.params["definition"] != json!(definition.id) {
        return Ok(Err((
            StatusCode::FORBIDDEN,
            format!(
                "sloppod: {name} was redefined since it was granted; the user can grant it again\n"
            ),
        )));
    }
    Ok(Ok(Upstream {
        mcp_url: definition.mcp_url,
        credential_id: definition.credential_id,
    }))
}

/// Where a request goes: the server's own URL for the bare path, and paths below it for the rest (a separate SSE
/// endpoint, say). Path and query are kept apart: the request's query follows the URL's own, if it has one.
fn upstream_url(mcp_url: &str, below: &str, query: Option<&str>) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(mcp_url)?;
    let below = match below {
        "" | "/" => String::new(),
        below => crate::proxy::canonical_path(below)
            .map_err(|e| anyhow::anyhow!("an invalid path: {e}"))?,
    };
    if !below.is_empty() {
        let path = format!("{}{below}", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    if let Some(query) = query.filter(|q| !q.is_empty()) {
        let joined = match url.query() {
            Some(own) if !own.is_empty() => format!("{own}&{query}"),
            _ => query.to_owned(),
        };
        url.set_query(Some(&joined));
    }
    Ok(url.into())
}

/// The header a `token` credential is sent as: `Authorization: Bearer <token>`, or its own header if it names one.
async fn token_header(
    state: &AppState,
    owner: &str,
    credential: Uuid,
) -> anyhow::Result<(String, String)> {
    let (row, secret) = state
        .credentials
        .token(owner, credential)
        .await?
        .context("the server's credential is gone")?;
    Ok(match row.config("header") {
        Some(header) => (header.to_owned(), secret),
        None => ("authorization".to_owned(), format!("Bearer {secret}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(name: &str, url: &str) -> NewDefinition {
        NewDefinition {
            name: name.into(),
            description: "Production logs".into(),
            mcp_url: url.into(),
            credential_id: None,
        }
    }

    #[test]
    fn a_definition_needs_a_free_name_and_a_server() {
        assert_eq!(
            invalid(&new(
                "victorialogs",
                "http://vl-mcp.monitoring.svc:8080/mcp"
            )),
            None
        );
        for (name, url) in [
            ("docs", "https://x/mcp"),
            ("sloppod", "https://x/mcp"),
            ("Logs", "https://x/mcp"),
            ("logs!", "https://x/mcp"),
            ("9logs", "https://x/mcp"),
            ("logs", "ftp://x/mcp"),
            ("logs", "not a url"),
            ("logs", "https://user:pass@x/mcp"),
        ] {
            assert!(invalid(&new(name, url)).is_some(), "{name} {url}");
        }
        let mut blank = new("logs", "https://x/mcp");
        blank.description = " ".into();
        assert!(invalid(&blank).is_some());
    }

    #[test]
    fn requests_go_to_the_server_s_url_with_their_path_and_query_kept_apart() {
        let to =
            |url: &str, below: &str, query: Option<&str>| upstream_url(url, below, query).unwrap();
        assert_eq!(to("https://x/mcp", "", None), "https://x/mcp");
        assert_eq!(to("https://x/mcp", "/", None), "https://x/mcp");
        assert_eq!(
            to("https://x/mcp", "", Some("cursor=1")),
            "https://x/mcp?cursor=1"
        );
        assert_eq!(
            to("https://x/mcp", "/messages", None),
            "https://x/mcp/messages"
        );
        assert_eq!(
            to("https://x/mcp/", "/messages", Some("a=b")),
            "https://x/mcp/messages?a=b"
        );
        assert_eq!(
            to("https://x/mcp?tenant=a", "/messages", None),
            "https://x/mcp/messages?tenant=a"
        );
        assert_eq!(
            to("https://x/mcp?tenant=a", "", Some("cursor=1")),
            "https://x/mcp?tenant=a&cursor=1"
        );
        assert_eq!(
            to("http://vl.svc:8080/mcp", "/%6Dsg", None),
            "http://vl.svc:8080/mcp/msg"
        );
        assert!(upstream_url("https://x/mcp", "/../admin", None).is_err());
    }

    #[test]
    fn grants_of_definitions_become_the_agent_s_servers() {
        let definition = Definition {
            id: Uuid::nil(),
            name: "victorialogs".into(),
            description: "Production logs".into(),
            mcp_url: "http://vl:8080/mcp".into(),
            credential_id: None,
            created_at: Utc::now(),
        };
        let grants = vec![
            Grant::new(sloppod_contracts::capability::DOCS, json!({})),
            Grant::new(sloppod_contracts::capability::GITHUB, json!({})),
            grant(&definition),
        ];
        assert_eq!(
            servers(&grants, "http://127.0.0.1:7777"),
            [
                (
                    "docs".to_owned(),
                    "http://127.0.0.1:7777/servers/docs".to_owned()
                ),
                (
                    "victorialogs".to_owned(),
                    "http://127.0.0.1:7777/servers/victorialogs".to_owned()
                )
            ]
        );
        assert_eq!(crate::grants::describe(&grants[2]), "Production logs");
    }
}
