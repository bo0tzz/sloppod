//! What sessions on endpoint models spend (#461). Endpoints like LiteLLM price every call and keep a key's spend and
//! budget, which the key itself can read (`/key/info`), so the runtime reads it, the way it reads a ChatGPT login's
//! limits, and shows running dollars per session and per user. It knows nothing of the providers behind the endpoint.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sloppod_contracts::{ModelTarget, SessionContext};
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

/// Keeps a key's reading, taken when its request went out (`at`), unless a reading of a request sent later is kept
/// already: a poll whose answer comes in after a key's final reading doesn't replace it. Says whether it changed what
/// was kept.
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
    let kept = sqlx::query(
        "insert into endpoint_spend (key_id, owner, session_id, shared_name, spend, max_budget, observed_at) \
         values ($1, $2, $3, $4, $5, $6, $7) \
         on conflict (key_id) do update set owner = excluded.owner, session_id = excluded.session_id, \
           shared_name = excluded.shared_name, spend = excluded.spend, max_budget = excluded.max_budget, \
           observed_at = excluded.observed_at \
         where endpoint_spend.observed_at <= excluded.observed_at",
    )
    .bind(key_id)
    .bind(owner)
    .bind(session)
    .bind(shared_name)
    .bind(reading.spend)
    .bind(reading.max_budget)
    .bind(at)
    .execute(db)
    .await?
    .rows_affected()
        > 0;
    Ok(kept && before != Some((reading.spend, reading.max_budget)))
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
    // A session's own keys (one per endpoint it was on) add up to its spend; a shared key is its own line. Newest
    // first, so a session's line takes its latest key's budget and time.
    let mut keys: Vec<SpentKey> = Vec::new();
    for row in rows {
        let own = row
            .shared_name
            .is_none()
            .then_some(row.session_id)
            .flatten();
        if let Some(session) = own
            && let Some(line) = keys
                .iter_mut()
                .find(|k| k.session_id == Some(session) && k.key_id.starts_with("session:"))
        {
            line.spend += row.spend;
            continue;
        }
        keys.push(SpentKey {
            key_id: match own {
                Some(session) => format!("session:{session}"),
                None => row.key_id,
            },
            session_id: row.session_id,
            name: row
                .shared_name
                .or(row.title)
                .unwrap_or_else(|| "a session since purged".into()),
            spend: row.spend,
            max_budget: row.max_budget,
            observed_at: row.observed_at.to_rfc3339(),
        });
    }
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
        if let Some(true) = read_session(state, &row.context(), &mut read).await {
            changed.insert(row.owner.clone());
        }
    }
    for owner in changed {
        let _ = state.spending.changed.send(owner);
    }
}

/// Reads the spend of the key a session's endpoint grant uses, once more before it goes: the session stopped or was
/// purged, or moved off the endpoint (`session` as it was). Without it, what was spent since the last poll would be
/// lost with the key.
pub async fn read_last(state: &AppState, session: &SessionContext) {
    if let Some(true) = read_session(state, session, &mut HashSet::new()).await {
        let _ = state.spending.changed.send(session.owner.clone());
    }
}

/// Reads and keeps the spend of the key `session`'s endpoint grant uses, unless it's in `read` already. Says whether
/// that changed what was kept; `None` when there was nothing to read.
async fn read_session(
    state: &AppState,
    session: &SessionContext,
    read: &mut HashSet<String>,
) -> Option<bool> {
    let target = match state.providers.target(session).await {
        Ok(Some(target)) => target,
        Ok(None) => return None,
        Err(e) => {
            tracing::debug!(session = %session.session_id, error = %format!("{e:#}"), "finding a session's endpoint key for its spend");
            return None;
        }
    };
    if !read.insert(target.key_id.clone()) {
        return None;
    }
    let id: Option<Uuid> = session.session_id.parse().ok();
    match record(
        &state.db,
        &state.inference_reach,
        &target,
        &session.owner,
        id,
    )
    .await
    {
        Ok(changed) => changed,
        Err(e) => {
            tracing::debug!(session = %session.session_id, error = %format!("{e:#}"), "reading an endpoint key's spend");
            None
        }
    }
}

/// Reads a key's spend from its endpoint and keeps it, as the session's own (`session`) unless the key is shared.
/// Says whether that changed what was kept; `None` for an endpoint whose answer isn't key info.
pub async fn record(
    db: &PgPool,
    reach: &crate::proxy::Reach,
    target: &ModelTarget,
    owner: &str,
    session: Option<Uuid>,
) -> anyhow::Result<Option<bool>> {
    let url = format!("{}/key/info", target.base_url.trim_end_matches('/'));
    // When the request goes out: a later one's answer is the newer, whichever arrives first.
    let at = Utc::now();
    let answer = reach.get_json(&url, &target.key).await?;
    let Some(reading) = from_key_info(&answer) else {
        return Ok(None);
    };
    let session = session.filter(|_| target.shared.is_none());
    Ok(Some(
        store(
            db,
            &target.key_id,
            owner,
            session,
            target.shared.as_deref(),
            &reading,
            at,
        )
        .await?,
    ))
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
        let session_key = format!("litellm:sloppod-{id}-aaaaaaaaaa");
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
        // The session moved to another proxy: its key there is another, and its spend adds to the first's.
        store(
            &db,
            &format!("litellm:sloppod-{id}-bbbbbbbbbb"),
            "dev",
            Some(id),
            None,
            &reading(0.25, Some(5.0)),
            at,
        )
        .await
        .unwrap();
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
        assert_eq!(spend.total, 3.0);
        let mut keys: Vec<(String, f64, Option<f64>)> = spend
            .keys
            .into_iter()
            .map(|k| (k.name, k.spend, k.max_budget))
            .collect();
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            keys,
            [
                ("Port the parser".to_owned(), 1.0, Some(5.0)),
                ("litellm".to_owned(), 2.0, None)
            ]
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_session_that_ends_between_reads_keeps_what_it_spent(db: PgPool) {
        // Its key's last reading, taken before the key goes, is kept as the session's.
        let app = axum::Router::new().route(
            "/key/info",
            axum::routing::get(|| async {
                axum::Json(json!({"info": {"spend": 0.31, "max_budget": null}}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let reach = crate::proxy::Reach::local(std::time::Duration::from_secs(5));
        let id = Uuid::now_v7();
        crate::db::insert_session(
            &db,
            crate::db::NewSessionRow {
                id,
                owner: "dev",
                title: "Short task",
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
        let own = ModelTarget {
            base_url: base.clone(),
            key: "sk".into(),
            key_id: format!("litellm:sloppod-{id}-aaaaaaaaaa"),
            shared: None,
        };
        assert_eq!(
            record(&db, &reach, &own, "dev", Some(id)).await.unwrap(),
            Some(true)
        );
        let spend = list(&db, "dev").await.unwrap();
        assert_eq!(spend.total, 0.31);
        assert_eq!(spend.keys[0].key_id, format!("session:{id}"));
        // A shared key's spend is the key's, not the session's.
        let shared = ModelTarget {
            base_url: base,
            key: "sk".into(),
            key_id: "credential:1".into(),
            shared: Some("litellm".into()),
        };
        record(&db, &reach, &shared, "dev", Some(id)).await.unwrap();
        let spend = list(&db, "dev").await.unwrap();
        let line = spend
            .keys
            .iter()
            .find(|k| k.key_id == "credential:1")
            .unwrap();
        assert_eq!((line.session_id, line.name.as_str()), (None, "litellm"));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_poll_answered_after_a_key_s_final_reading_doesn_t_replace_it(db: PgPool) {
        let reading = |spend: f64| Reading {
            spend,
            max_budget: None,
        };
        let polled = Utc::now();
        let last = polled + chrono::Duration::seconds(1);
        // The final reading, sent after the poll's request, is kept first; the poll's answer comes in after it.
        assert!(
            store(
                &db,
                "credential:1",
                "dev",
                None,
                Some("litellm"),
                &reading(1.0),
                last
            )
            .await
            .unwrap()
        );
        assert!(
            !store(
                &db,
                "credential:1",
                "dev",
                None,
                Some("litellm"),
                &reading(0.5),
                polled
            )
            .await
            .unwrap()
        );
        assert_eq!(list(&db, "dev").await.unwrap().total, 1.0);
    }
}
