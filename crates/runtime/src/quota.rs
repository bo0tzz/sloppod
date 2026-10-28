//! What's left of each inference credential's usage limits (#140). Claude Code reports a Claude subscription's in its
//! sessions' `usage_update`s; a ChatGPT login's are read from ChatGPT, since codex-acp doesn't report them.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sloppod_protocol::api::{Quota, QuotaStatus, QuotaWindow};
use sqlx::PgPool;
use sqlx::types::Json;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::credentials::{CHATGPT, CLAUDE, CredentialRow};
use crate::{AppState, db};

/// Where the Codex CLI reads a ChatGPT login's limits.
const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// What one report says about a credential's limits.
#[derive(Debug, PartialEq)]
pub struct Observation {
    pub limited: bool,
    pub resets_at: Option<DateTime<Utc>>,
    /// The windows it reports; others keep what was last reported of them.
    pub windows: BTreeMap<String, Window>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub utilization: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
    pub status: QuotaStatus,
}

/// A credential's quota changed, for whoever needs to know at once.
#[derive(Debug, Clone)]
pub struct Change {
    pub owner: String,
    pub quota: Quota,
}

pub struct Quotas {
    http: reqwest::Client,
    changed: broadcast::Sender<Change>,
}

impl Quotas {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
            changed: broadcast::channel(64).0,
        })
    }

    /// Every credential's quota as it changes.
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.changed.subscribe()
    }
}

fn epoch(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(value.as_i64()?, 0)
}

fn claude_status(status: &str) -> Option<QuotaStatus> {
    match status {
        "allowed" => Some(QuotaStatus::Allowed),
        "allowed_warning" => Some(QuotaStatus::Warning),
        "rejected" => Some(QuotaStatus::Rejected),
        _ => None,
    }
}

/// A window's status where the report gives none.
fn status_of(utilization: Option<f64>) -> QuotaStatus {
    if utilization.is_some_and(|u| u >= 1.0) {
        QuotaStatus::Rejected
    } else {
        QuotaStatus::Allowed
    }
}

/// Claude Code's rate-limit info (`_meta["_claude/rateLimit"]` of a `usage_update`): its top-level fields are about
/// the window that's limiting now, and newer versions report every window in `unifiedWindows`.
pub fn from_claude(info: &Value) -> Option<Observation> {
    let status = claude_status(info["status"].as_str()?)?;
    let mut windows = BTreeMap::new();
    for (name, window) in info["unifiedWindows"].as_object().into_iter().flatten() {
        let utilization = window["utilization"].as_f64();
        windows.insert(
            name.clone(),
            Window {
                utilization,
                resets_at: epoch(&window["resetsAt"]),
                status: window["status"]
                    .as_str()
                    .and_then(claude_status)
                    .unwrap_or_else(|| status_of(utilization)),
            },
        );
    }
    if let (Some(name), Some(utilization)) =
        (info["rateLimitType"].as_str(), info["utilization"].as_f64())
    {
        windows.entry(name.to_owned()).or_insert(Window {
            utilization: Some(utilization),
            resets_at: epoch(&info["resetsAt"]),
            status,
        });
    }
    let limited = status == QuotaStatus::Rejected;
    Some(Observation {
        limited,
        resets_at: if limited {
            epoch(&info["resetsAt"])
        } else {
            None
        },
        windows,
    })
}

/// A window's name from its length: the session and weekly limits by the names Claude's have.
fn window_name(seconds: i64) -> String {
    match seconds {
        18_000 => "five_hour".into(),
        604_800 => "seven_day".into(),
        s if s % 86_400 == 0 => format!("{}_day", s / 86_400),
        s if s % 3_600 == 0 => format!("{}_hour", s / 3_600),
        s => format!("{}_minute", s / 60),
    }
}

/// ChatGPT's usage report for a login (`/wham/usage`); `None` for one without limits.
pub fn from_codex(usage: &Value, now: DateTime<Utc>) -> Option<Observation> {
    let limit = usage.get("rate_limit").filter(|l| l.is_object())?;
    let mut windows = BTreeMap::new();
    for (key, fallback) in [
        ("primary_window", "primary"),
        ("secondary_window", "secondary"),
    ] {
        let window = &limit[key];
        let Some(used) = window["used_percent"].as_f64() else {
            continue;
        };
        let name = window["limit_window_seconds"]
            .as_i64()
            .map_or_else(|| fallback.to_owned(), window_name);
        let utilization = used / 100.0;
        windows.insert(
            name,
            Window {
                utilization: Some(utilization),
                resets_at: epoch(&window["reset_at"]).or_else(|| {
                    window["reset_after_seconds"]
                        .as_i64()
                        .map(|s| now + Duration::seconds(s))
                }),
                status: status_of(Some(utilization)),
            },
        );
    }
    let limited = limit["limit_reached"] == true || limit["allowed"] == false;
    let resets_at = windows
        .values()
        .filter(|w| w.status == QuotaStatus::Rejected)
        .filter_map(|w| w.resets_at)
        .max();
    Some(Observation {
        limited,
        resets_at: resets_at.filter(|_| limited),
        windows,
    })
}

/// Keeps what a report says of a credential's limits, over what earlier ones said.
pub async fn store(
    db: &PgPool,
    credential: Uuid,
    observation: &Observation,
    observed_at: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query(
        "insert into credential_quotas (credential_id, limited, resets_at, windows, observed_at) \
         values ($1, $2, $3, $4, $5) \
         on conflict (credential_id) do update set limited = excluded.limited, resets_at = excluded.resets_at, \
         windows = credential_quotas.windows || excluded.windows, observed_at = excluded.observed_at",
    )
    .bind(credential)
    .bind(observation.limited)
    .bind(observation.resets_at)
    .bind(Json(&observation.windows))
    .bind(observed_at)
    .execute(db)
    .await
    .map(|_| ())
}

#[derive(sqlx::FromRow)]
struct QuotaRow {
    credential_id: Uuid,
    kind: String,
    name: String,
    limited: bool,
    resets_at: Option<DateTime<Utc>>,
    windows: Json<BTreeMap<String, Window>>,
    observed_at: DateTime<Utc>,
}

impl QuotaRow {
    /// As it stands at `now`: a limit whose reset has passed is over, and a window that has reset since is unused.
    fn into_quota(self, now: DateTime<Utc>) -> Quota {
        let over = |at: Option<DateTime<Utc>>| at.is_some_and(|at| at <= now);
        let (limited, resets_at) = if over(self.resets_at) {
            (false, None)
        } else {
            (self.limited, self.resets_at)
        };
        Quota {
            credential_id: self.credential_id,
            kind: self.kind,
            name: self.name,
            limited,
            resets_at: resets_at.map(|at| at.to_rfc3339()),
            windows: self
                .windows
                .0
                .into_iter()
                .map(|(window, w)| {
                    if over(w.resets_at) {
                        QuotaWindow {
                            window,
                            utilization: Some(0.0),
                            resets_at: None,
                            status: QuotaStatus::Allowed,
                        }
                    } else {
                        QuotaWindow {
                            window,
                            utilization: w.utilization,
                            resets_at: w.resets_at.map(|at| at.to_rfc3339()),
                            status: w.status,
                        }
                    }
                })
                .collect(),
            observed_at: self.observed_at.to_rfc3339(),
        }
    }
}

const SELECT: &str = "select q.credential_id, c.kind, c.name, q.limited, q.resets_at, q.windows, q.observed_at \
                      from credential_quotas q join credentials c on c.id = q.credential_id";

/// The quotas of an owner's credentials that have had one reported, as they stand at `now`.
pub async fn list(db: &PgPool, owner: &str, now: DateTime<Utc>) -> sqlx::Result<Vec<Quota>> {
    let rows: Vec<QuotaRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} where c.owner = $1 order by c.kind, c.name"
    )))
    .bind(owner)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|row| row.into_quota(now)).collect())
}

/// A credential's quota as it stands at `now`, if one was reported.
pub async fn for_credential(
    db: &PgPool,
    credential: Uuid,
    now: DateTime<Utc>,
) -> sqlx::Result<Option<Quota>> {
    let row: Option<QuotaRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} where q.credential_id = $1"
    )))
    .bind(credential)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| row.into_quota(now)))
}

async fn record(state: &AppState, credential: &CredentialRow, observation: Observation) {
    let now = Utc::now();
    if let Err(e) = store(&state.db, credential.id, &observation, now).await {
        tracing::warn!(credential = %credential.id, error = %e, "keeping a credential's quota");
        return;
    }
    if let Ok(Some(quota)) = for_credential(&state.db, credential.id, now).await {
        let _ = state.quotas.changed.send(Change {
            owner: credential.owner.clone(),
            quota,
        });
    }
}

/// A `usage_update` from a session's agent: Claude Code's carry the rate limits of the Claude subscription the
/// session used, its owner's default.
pub async fn seen(state: &AppState, session: Uuid, update: Value) {
    let Some(observation) = from_claude(&update["_meta"]["_claude/rateLimit"]) else {
        return;
    };
    let credential = match db::get_session(&state.db, session).await {
        Ok(Some(session)) => state.credentials.default_row(&session.owner, CLAUDE).await,
        Ok(None) => return,
        Err(e) => Err(e),
    };
    match credential {
        Ok(Some(credential)) => record(state, &credential, observation).await,
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(%session, error = %e, "finding the Claude credential a session used")
        }
    }
}

/// Reads the limits of every owner's default ChatGPT login.
pub async fn read_chatgpt(state: &AppState) {
    let owners = match state.credentials.owners_with_default(CHATGPT).await {
        Ok(owners) => owners,
        Err(e) => {
            tracing::warn!(error = %e, "listing ChatGPT logins");
            return;
        }
    };
    for owner in owners {
        if let Err(e) = read_chatgpt_of(state, &owner).await {
            tracing::warn!(%owner, error = %e, "reading a ChatGPT login's limits");
        }
    }
}

async fn read_chatgpt_of(state: &AppState, owner: &str) -> anyhow::Result<()> {
    let (Some(credential), Some(tokens)) = (
        state.credentials.default_row(owner, CHATGPT).await?,
        state.credentials.chatgpt(owner).await?,
    ) else {
        return Ok(());
    };
    let usage: Value = state
        .quotas
        .http
        .get(CHATGPT_USAGE_URL)
        .bearer_auth(&tokens.access_token)
        .header("chatgpt-account-id", &tokens.account_id)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if let Some(observation) = from_codex(&usage, Utc::now()) {
        record(state, &credential, observation).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn claude_reports_every_window_and_whether_it_is_refused() {
        let info = json!({
            "status": "rejected",
            "resetsAt": 1_793_190_000,
            "rateLimitType": "five_hour",
            "utilization": 1.0,
            "unifiedWindows": {
                "five_hour": {"utilization": 1.02, "resetsAt": 1_793_190_000, "status": "rejected", "surpassedThreshold": 1},
                "seven_day": {"utilization": 0.62, "resetsAt": 1_793_620_000, "status": "allowed_warning"},
            },
        });
        let observation = from_claude(&info).unwrap();
        assert!(observation.limited);
        assert_eq!(
            observation.resets_at,
            DateTime::from_timestamp(1_793_190_000, 0)
        );
        assert_eq!(observation.windows.len(), 2);
        assert_eq!(
            observation.windows["five_hour"].status,
            QuotaStatus::Rejected
        );
        assert_eq!(
            observation.windows["seven_day"],
            Window {
                utilization: Some(0.62),
                resets_at: DateTime::from_timestamp(1_793_620_000, 0),
                status: QuotaStatus::Warning,
            }
        );
    }

    #[test]
    fn an_older_claude_reports_only_the_limiting_window() {
        let info = json!({"status": "allowed", "resetsAt": 1_793_190_000, "rateLimitType": "seven_day", "utilization": 0.4});
        let observation = from_claude(&info).unwrap();
        assert!(!observation.limited);
        assert_eq!(observation.resets_at, None);
        assert_eq!(
            observation.windows.keys().collect::<Vec<_>>(),
            ["seven_day"]
        );
        assert_eq!(observation.windows["seven_day"].utilization, Some(0.4));
        // A status alone says nothing of any window.
        let observation = from_claude(&json!({"status": "allowed"})).unwrap();
        assert!(observation.windows.is_empty());
        assert_eq!(from_claude(&Value::Null), None);
    }

    #[test]
    fn chatgpt_reports_its_windows_by_length() {
        let now = at("2026-10-28T12:00:00Z");
        let usage = json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": false,
                "limit_reached": true,
                "primary_window": {"used_percent": 100.0, "limit_window_seconds": 18_000, "reset_after_seconds": 3_600},
                "secondary_window": {"used_percent": 41.5, "limit_window_seconds": 604_800, "reset_at": 1_793_620_000},
            },
        });
        let observation = from_codex(&usage, now).unwrap();
        assert!(observation.limited);
        assert_eq!(observation.resets_at, Some(at("2026-10-28T13:00:00Z")));
        assert_eq!(
            observation.windows["five_hour"].status,
            QuotaStatus::Rejected
        );
        assert_eq!(
            observation.windows["seven_day"],
            Window {
                utilization: Some(0.415),
                resets_at: DateTime::from_timestamp(1_793_620_000, 0),
                status: QuotaStatus::Allowed,
            }
        );
        assert_eq!(window_name(7_200), "2_hour");
        assert_eq!(
            from_codex(&json!({"plan_type": "free", "rate_limit": null}), now),
            None
        );
    }

    #[test]
    fn a_quota_reads_as_of_now() {
        let row = || QuotaRow {
            credential_id: Uuid::nil(),
            kind: CLAUDE.into(),
            name: "work".into(),
            limited: true,
            resets_at: Some(at("2026-10-28T13:00:00Z")),
            windows: Json(BTreeMap::from([
                (
                    "five_hour".into(),
                    Window {
                        utilization: Some(1.0),
                        resets_at: Some(at("2026-10-28T13:00:00Z")),
                        status: QuotaStatus::Rejected,
                    },
                ),
                (
                    "seven_day".into(),
                    Window {
                        utilization: Some(0.7),
                        resets_at: Some(at("2026-11-01T00:00:00Z")),
                        status: QuotaStatus::Allowed,
                    },
                ),
            ])),
            observed_at: at("2026-10-28T12:00:00Z"),
        };
        let before = row().into_quota(at("2026-10-28T12:30:00Z"));
        assert!(before.limited);
        assert_eq!(before.windows[0].utilization, Some(1.0));

        let after = row().into_quota(at("2026-10-28T13:00:00Z"));
        assert!(!after.limited);
        assert_eq!(after.resets_at, None);
        assert_eq!(
            after.windows[0],
            QuotaWindow {
                window: "five_hour".into(),
                utilization: Some(0.0),
                resets_at: None,
                status: QuotaStatus::Allowed,
            }
        );
        assert_eq!(after.windows[1].utilization, Some(0.7));
    }
}
