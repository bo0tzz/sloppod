//! What sessions on endpoint models spend (#461). Endpoints like LiteLLM price every call and keep a key's spend and
//! budget, which the key itself can read (`/key/info`), so the runtime reads it, the way it reads a ChatGPT login's
//! limits, and shows running dollars per session and per user. It knows nothing of the providers behind the endpoint.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sloppod_protocol::api::{Spend, SpentKey};
use sqlx::PgPool;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{AppState, db};

/// What a key's info says it has spent, in dollars, and the most it may.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub spend: f64,
    pub max_budget: Option<f64>,
}

/// LiteLLM's key info (`GET /key/info` with the key): `{key, info: {spend, max_budget, ...}}`. `None` for an answer
/// that isn't one, e.g. from an endpoint that isn't LiteLLM.
pub fn from_key_info(answer: &Value) -> Option<Reading> {
    let info = &answer["info"];
    Some(Reading {
        spend: info["spend"].as_f64()?,
        max_budget: info["max_budget"].as_f64(),
    })
}

/// A user's spend changed, for their UIs (`_sloppod/spend`).
pub struct Spending {
    changed: broadcast::Sender<String>,
}

impl Default for Spending {
    fn default() -> Self {
        Self {
            changed: broadcast::channel(64).0,
        }
    }
}

impl Spending {
    /// The owners whose spend changes, as it does.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.changed.subscribe()
    }
}

/// Keeps a key's reading; says whether it changed what was kept.
pub async fn store(
    db: &PgPool,
    key_id: &str,
    owner: &str,
    session: Option<Uuid>,
    shared_name: Option<&str>,
    reading: &Reading,
    at: DateTime<Utc>,
) -> sqlx::Result<bool> {
    let before: Option<(f64, Option<f64>)> =
        sqlx::query_as("select spend, max_budget from endpoint_spend where key_id = $1")
            .bind(key_id)
            .fetch_optional(db)
            .await?;
    sqlx::query(
        "insert into endpoint_spend (key_id, owner, session_id, shared_name, spend, max_budget, observed_at) \
         values ($1, $2, $3, $4, $5, $6, $7) \
         on conflict (key_id) do update set owner = excluded.owner, session_id = excluded.session_id, \
           shared_name = excluded.shared_name, spend = excluded.spend, max_budget = excluded.max_budget, \
           observed_at = excluded.observed_at",
    )
    .bind(key_id)
    .bind(owner)
    .bind(session)
    .bind(shared_name)
    .bind(reading.spend)
    .bind(reading.max_budget)
    .bind(at)
    .execute(db)
    .await?;
    Ok(before != Some((reading.spend, reading.max_budget)))
}

/// The owner's spend: every key's their sessions used, newest first, and the sum.
pub async fn list(db: &PgPool, owner: &str) -> sqlx::Result<Spend> {
    #[derive(sqlx::FromRow)]
    struct Row {
        key_id: String,
        session_id: Option<Uuid>,
        shared_name: Option<String>,
        title: Option<String>,
        spend: f64,
        max_budget: Option<f64>,
        observed_at: DateTime<Utc>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "select e.key_id, e.session_id, e.shared_name, s.title, e.spend, e.max_budget, e.observed_at \
         from endpoint_spend e left join sessions s on s.id = e.session_id \
         where e.owner = $1 order by e.observed_at desc, e.key_id",
    )
    .bind(owner)
    .fetch_all(db)
    .await?;
    let keys: Vec<SpentKey> = rows
        .into_iter()
        .map(|row| SpentKey {
            key_id: row.key_id,
            session_id: row.session_id,
            name: row
                .shared_name
                .or(row.title)
                .unwrap_or_else(|| "a session since purged".into()),
            spend: row.spend,
            max_budget: row.max_budget,
            observed_at: row.observed_at.to_rfc3339(),
        })
        .collect();
    Ok(Spend {
        total: keys.iter().map(|k| k.spend).sum(),
        keys,
    })
}

/// Reads the spend of the keys the sessions on endpoint models use, each key once, and keeps what they say. Errors
/// are logged: an endpoint without key info (one that isn't LiteLLM) just has no spend to show.
pub async fn poll(state: &AppState) {
    let held = match db::endpoint_grants_held(&state.db).await {
        Ok(held) => held,
        Err(e) => {
            tracing::warn!(error = %e, "listing sessions on endpoint models for their spend");
            return;
        }
    };
    let mut read = HashSet::new();
    let mut changed = HashSet::new();
    for (session, _) in held {
        let Ok(id) = session.parse::<Uuid>() else {
            continue;
        };
        let Ok(Some(row)) = db::get_session(&state.db, id).await else {
            continue;
        };
        let target = match state.providers.target(&row.context()).await {
            Ok(Some(target)) => target,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(%session, error = %format!("{e:#}"), "finding a session's endpoint key for its spend");
                continue;
            }
        };
        let (key_id, owned_by, shared_name) = match &target.shared {
            Some(shared) => (shared.id.clone(), None, Some(shared.name.clone())),
            None => (format!("session:{id}"), Some(id), None),
        };
        if !read.insert(key_id.clone()) {
            continue;
        }
        let url = format!("{}/key/info", target.base_url.trim_end_matches('/'));
        let answer = match state.inference_reach.get_json(&url, &target.key).await {
            Ok(answer) => answer,
            Err(e) => {
                tracing::debug!(%session, error = %format!("{e:#}"), "reading an endpoint key's spend");
                continue;
            }
        };
        let Some(reading) = from_key_info(&answer) else {
            continue;
        };
        match store(
            &state.db,
            &key_id,
            &row.owner,
            owned_by,
            shared_name.as_deref(),
            &reading,
            Utc::now(),
        )
        .await
        {
            Ok(true) => {
                changed.insert(row.owner.clone());
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(%session, error = %e, "keeping an endpoint key's spend"),
        }
    }
    for owner in changed {
        let _ = state.spending.changed.send(owner);
    }
}

/// Sends an owner's UI connection a `_sloppod/spend` notification whenever their spend changes, for it to read it
/// again. Ends when the connection does; one that falls behind is told to reconnect (`lagged`).
pub async fn forward(
    owner: &str,
    mut changes: broadcast::Receiver<String>,
    out: tokio::sync::mpsc::Sender<Value>,
    lagged: impl Fn(),
) {
    loop {
        let changed = match changes.recv().await {
            Ok(changed) => changed,
            Err(broadcast::error::RecvError::Lagged(_)) => return lagged(),
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if changed != owner {
            continue;
        }
        let notification =
            serde_json::json!({"jsonrpc": "2.0", "method": "_sloppod/spend", "params": {}});
        if out.send(notification).await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_key_s_info_says_what_it_spent_and_may() {
        let answer = json!({"key": "hashed", "info": {"spend": 1.2345, "max_budget": 5.0, "key_alias": "sloppod-s"}});
        assert_eq!(
            from_key_info(&answer),
            Some(Reading {
                spend: 1.2345,
                max_budget: Some(5.0)
            })
        );
        let unlimited = json!({"info": {"spend": 0, "max_budget": null}});
        assert_eq!(
            from_key_info(&unlimited),
            Some(Reading {
                spend: 0.0,
                max_budget: None
            })
        );
        // Not LiteLLM's answer: nothing to show.
        assert_eq!(from_key_info(&json!({"error": "not found"})), None);
    }

    #[tokio::test]
    async fn a_key_s_spend_is_read_with_the_key_itself() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let saw = seen.clone();
        let app = axum::Router::new().route(
            "/key/info",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                *saw.lock().unwrap() = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                async {
                    axum::Json(json!({"key": "hashed", "info": {"spend": 0.42, "max_budget": 2.0}}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://127.0.0.1:{}/key/info",
            listener.local_addr().unwrap().port()
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let reach = crate::proxy::Reach::local(std::time::Duration::from_secs(5));
        let answer = reach.get_json(&url, "sk-session").await.unwrap();
        assert_eq!(
            from_key_info(&answer),
            Some(Reading {
                spend: 0.42,
                max_budget: Some(2.0)
            })
        );
        assert_eq!(seen.lock().unwrap().as_deref(), Some("Bearer sk-session"));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn spend_is_kept_per_key_and_summed_per_user(db: PgPool) {
        let id = Uuid::now_v7();
        crate::db::insert_session(
            &db,
            crate::db::NewSessionRow {
                id,
                owner: "dev",
                title: "Port the parser",
                custom_title: true,
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
        let at = Utc::now();
        let reading = |spend: f64, max_budget: Option<f64>| Reading { spend, max_budget };
        let session_key = format!("session:{id}");
        assert!(
            store(
                &db,
                &session_key,
                "dev",
                Some(id),
                None,
                &reading(0.5, Some(5.0)),
                at
            )
            .await
            .unwrap()
        );
        // The same again isn't a change; more spent is.
        assert!(
            !store(
                &db,
                &session_key,
                "dev",
                Some(id),
                None,
                &reading(0.5, Some(5.0)),
                at
            )
            .await
            .unwrap()
        );
        assert!(
            store(
                &db,
                &session_key,
                "dev",
                Some(id),
                None,
                &reading(0.75, Some(5.0)),
                at
            )
            .await
            .unwrap()
        );
        store(
            &db,
            "credential:1",
            "dev",
            None,
            Some("litellm"),
            &reading(2.0, None),
            at,
        )
        .await
        .unwrap();
        store(
            &db,
            "credential:2",
            "someone",
            None,
            Some("theirs"),
            &reading(9.0, None),
            at,
        )
        .await
        .unwrap();

        let spend = list(&db, "dev").await.unwrap();
        assert_eq!(spend.total, 2.75);
        let mut keys: Vec<(String, f64, Option<f64>)> = spend
            .keys
            .into_iter()
            .map(|k| (k.name, k.spend, k.max_budget))
            .collect();
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            keys,
            [
                ("Port the parser".to_owned(), 0.75, Some(5.0)),
                ("litellm".to_owned(), 2.0, None)
            ]
        );
    }
}
