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

/// How long a limit whose reset nobody reported lasts: after it, the credential is tried again (#251, #252).
pub const PROBE_AFTER: Duration = Duration::minutes(30);

/// Where the Codex CLI reads a ChatGPT login's limits.
const CHATGPT_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// What one report says about a credential's limits.
#[derive(Debug, PartialEq)]
pub struct Observation {
    /// When the report came; an older one never replaces what a newer one said.
    pub observed_at: DateTime<Utc>,
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

/// A window as kept, with when it was reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Kept {
    #[serde(flatten)]
    window: Window,
    observed_at: DateTime<Utc>,
}

/// A credential's quota changed, for whoever needs to know at once: the session waits on usage limits, and the
/// owner's UIs (`_sloppod/quota`).
#[derive(Debug, Clone)]
pub struct Change {
    pub owner: String,
    pub credential_id: Uuid,
    /// As it stands now; `None` once the credential is gone.
    pub quota: Option<Quota>,
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

    /// Tells whoever follows quotas that a credential's quota is `quota` now, or gone.
    pub fn send(&self, owner: &str, credential_id: Uuid, quota: Option<Quota>) {
        let _ = self.changed.send(Change {
            owner: owner.to_owned(),
            credential_id,
            quota,
        });
    }
}

/// Sends an owner's UI connection each change of `owner`'s quotas, as a `_sloppod/quota` notification:
/// `{credentialId, quota}`, `quota` null once the credential is gone. Ends when the connection does; a connection that
/// falls behind is told to reconnect (`lagged`), so it fetches them all again rather than miss one.
pub async fn forward(
    owner: &str,
    mut changes: broadcast::Receiver<Change>,
    out: tokio::sync::mpsc::Sender<Value>,
    lagged: impl Fn(),
) {
    loop {
        let change = match changes.recv().await {
            Ok(change) => change,
            Err(broadcast::error::RecvError::Lagged(_)) => return lagged(),
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if change.owner != owner {
            continue;
        }
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "_sloppod/quota",
            "params": {"credentialId": change.credential_id, "quota": change.quota},
        });
        if out.send(notification).await.is_err() {
            return;
        }
    }
}

/// Announces quotas when a reset passes: a limit that ends, or a window that starts over, changes what the quota
/// says without anything being reported. Sleeps until the earliest reset of any quota, and plans again on every
/// change, which may bring an earlier one.
pub fn announce_resets(state: &AppState) {
    let state = state.clone();
    let mut changes = state.quotas.subscribe();
    tokio::spawn(async move {
        loop {
            let now = Utc::now();
            let planned = match all(&state.db, now).await {
                Ok(quotas) => quotas,
                Err(e) => {
                    tracing::warn!(error = %e, "listing quotas for their resets");
                    Vec::new()
                }
            };
            let next = planned.iter().filter_map(|(_, q)| next_reset(q)).min();
            // Without a reset to wait for, a change still wakes it; once an hour it looks again regardless.
            let wait = next
                .map(|at| (at - now).to_std().unwrap_or_default())
                .unwrap_or(std::time::Duration::from_secs(3600))
                + std::time::Duration::from_secs(1);
            let closed = tokio::select! {
                _ = tokio::time::sleep(wait) => false,
                change = changes.recv() => matches!(change, Err(broadcast::error::RecvError::Closed)),
            };
            // However it woke: what's planned next is listed as it stands then, which no longer shows a reset that
            // has passed, so a reset that passed meanwhile is announced now or never.
            let now = Utc::now();
            for (owner, quota) in due(&planned, now) {
                if let Ok(fresh) = for_credential(&state.db, quota.credential_id, now).await {
                    state.quotas.send(owner, quota.credential_id, fresh);
                }
            }
            if closed {
                return;
            }
        }
    });
}

/// The planned quotas a reset of which has passed by `now`.
fn due(planned: &[(String, Quota)], now: DateTime<Utc>) -> impl Iterator<Item = &(String, Quota)> {
    planned
        .iter()
        .filter(move |(_, quota)| next_reset(quota).is_some_and(|at| at <= now))
}

/// The earliest reset a quota still has to come: its limit's end, or a window's.
fn next_reset(quota: &Quota) -> Option<DateTime<Utc>> {
    let parse = |at: &Option<String>| {
        at.as_deref()
            .and_then(|at| at.parse::<DateTime<Utc>>().ok())
    };
    std::iter::once(parse(&quota.resets_at))
        .chain(quota.windows.iter().map(|w| parse(&w.resets_at)))
        .flatten()
        .min()
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
pub fn from_claude(info: &Value, observed_at: DateTime<Utc>) -> Option<Observation> {
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
        observed_at,
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

/// ChatGPT's usage report for a login (`/wham/usage`): `None` for one without limits, and an error for a report this
/// doesn't recognise, which is better ignored than taken for one that says the login isn't limited.
pub fn from_codex(usage: &Value, now: DateTime<Utc>) -> Result<Option<Observation>, String> {
    let limit = match usage.get("rate_limit") {
        Some(Value::Null) => return Ok(None),
        Some(limit) if limit.is_object() => limit,
        _ => return Err("the report has no `rate_limit`".into()),
    };
    let (Some(allowed), Some(reached)) =
        (limit["allowed"].as_bool(), limit["limit_reached"].as_bool())
    else {
        return Err("`rate_limit` doesn't say whether the limit is reached".into());
    };
    let mut windows = BTreeMap::new();
    for key in ["primary_window", "secondary_window"] {
        let window = &limit[key];
        if window.is_null() {
            continue;
        }
        let (Some(used), Some(seconds)) = (
            window["used_percent"].as_f64(),
            window["limit_window_seconds"].as_i64(),
        ) else {
            return Err(format!(
                "`{key}` has no `used_percent` or `limit_window_seconds`"
            ));
        };
        let resets_at = epoch(&window["reset_at"])
            .or_else(|| {
                window["reset_after_seconds"]
                    .as_i64()
                    .map(|s| now + Duration::seconds(s))
            })
            .ok_or_else(|| format!("`{key}` doesn't say when it resets"))?;
        let utilization = used / 100.0;
        windows.insert(
            window_name(seconds),
            Window {
                utilization: Some(utilization),
                resets_at: Some(resets_at),
                status: status_of(Some(utilization)),
            },
        );
    }
    let limited = reached || !allowed;
    let resets_at = windows
        .values()
        .filter(|w| w.status == QuotaStatus::Rejected)
        .filter_map(|w| w.resets_at)
        .max();
    Ok(Some(Observation {
        observed_at: now,
        limited,
        resets_at: resets_at.filter(|_| limited),
        windows,
    }))
}

/// What's kept of a credential's quota.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct State {
    limited: bool,
    resets_at: Option<DateTime<Utc>>,
    windows: Json<BTreeMap<String, Kept>>,
    observed_at: DateTime<Utc>,
}

/// When a limit ends: at its reset, or, when nobody said when that is, [`PROBE_AFTER`] after it was reported, when
/// the credential is tried again.
fn limit_ends(
    limited: bool,
    resets_at: Option<DateTime<Utc>>,
    observed_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match resets_at {
        None if limited => Some(observed_at + PROBE_AFTER),
        known => known,
    }
}

impl State {
    fn limited_at(&self, now: DateTime<Utc>) -> bool {
        self.limited
            && limit_ends(self.limited, self.resets_at, self.observed_at)
                .is_none_or(|end| end > now)
    }
}

/// A report over what's kept: it replaces only what no newer report has said, whatever order they arrive in.
fn merge(mut state: State, observation: &Observation) -> State {
    let at = observation.observed_at;
    if at >= state.observed_at {
        state.limited = observation.limited;
        state.resets_at = observation.resets_at;
        state.observed_at = at;
    }
    for (name, window) in &observation.windows {
        if state
            .windows
            .get(name)
            .is_none_or(|kept| at >= kept.observed_at)
        {
            state.windows.insert(
                name.clone(),
                Kept {
                    window: window.clone(),
                    observed_at: at,
                },
            );
        }
    }
    state
}

/// Keeps what a report says of a credential's limits, merged with what's kept under the row's lock, so concurrent
/// reports can't undo each other.
pub async fn store(db: &PgPool, credential: Uuid, observation: &Observation) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    // An empty row older than any report, so a credential's first two reports lock the same row too.
    sqlx::query(
        "insert into credential_quotas (credential_id, limited, windows, observed_at) \
         values ($1, false, '{}', to_timestamp(0)) on conflict (credential_id) do nothing",
    )
    .bind(credential)
    .execute(&mut *tx)
    .await?;
    let current: State = sqlx::query_as(
        "select limited, resets_at, windows, observed_at from credential_quotas where credential_id = $1 \
         for update",
    )
    .bind(credential)
    .fetch_one(&mut *tx)
    .await?;
    let state = merge(current, observation);
    sqlx::query(
        "update credential_quotas set limited = $2, resets_at = $3, windows = $4, observed_at = $5 \
         where credential_id = $1",
    )
    .bind(credential)
    .bind(state.limited)
    .bind(state.resets_at)
    .bind(&state.windows)
    .bind(state.observed_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

#[derive(sqlx::FromRow)]
struct QuotaRow {
    credential_id: Uuid,
    kind: String,
    name: String,
    limited: bool,
    resets_at: Option<DateTime<Utc>>,
    windows: Json<BTreeMap<String, Kept>>,
    observed_at: DateTime<Utc>,
}

impl QuotaRow {
    /// As it stands at `now`: a limit whose reset has passed is over, and a window that has reset since is unused.
    fn into_quota(self, now: DateTime<Utc>) -> Quota {
        let over = |at: Option<DateTime<Utc>>| at.is_some_and(|at| at <= now);
        // A limit whose reset isn't known reads as resetting when it's tried again, and a wait for it ends then too.
        let resets_at = limit_ends(self.limited, self.resets_at, self.observed_at);
        let (limited, resets_at) = if over(resets_at) {
            (false, None)
        } else {
            (self.limited, resets_at)
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
                .map(|(window, Kept { window: w, .. })| {
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

/// Every quota that has been reported, with its credential's owner, as it stands at `now`.
pub(crate) async fn all(db: &PgPool, now: DateTime<Utc>) -> sqlx::Result<Vec<(String, Quota)>> {
    #[derive(sqlx::FromRow)]
    struct Owned {
        owner: String,
        #[sqlx(flatten)]
        row: QuotaRow,
    }
    let rows: Vec<Owned> = sqlx::query_as(sqlx::AssertSqlSafe(SELECT.replacen(
        "select ",
        "select c.owner, ",
        1,
    )))
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|o| (o.owner, o.row.into_quota(now)))
        .collect())
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

/// Whether a credential is at its usage limit now, as far as its quota says.
pub async fn is_limited(db: &PgPool, credential: Uuid, now: DateTime<Utc>) -> sqlx::Result<bool> {
    Ok(for_credential(db, credential, now)
        .await?
        .is_some_and(|quota| quota.limited))
}

/// A turn failed on a credential's usage limit (#252): unless its quota already says it's limited, it's kept as
/// limited until [`PROBE_AFTER`] from now. That's the runtime's guess, not a report: it doesn't move when the quota
/// was last reported, so a report that came before the failure but is stored after it still replaces it, with the
/// reset it knows. Checked and written under the row's lock, like reports. Says whether it changed.
pub async fn mark_limited(db: &PgPool, credential: Uuid, now: DateTime<Utc>) -> sqlx::Result<bool> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "insert into credential_quotas (credential_id, limited, windows, observed_at) \
         values ($1, false, '{}', to_timestamp(0)) on conflict (credential_id) do nothing",
    )
    .bind(credential)
    .execute(&mut *tx)
    .await?;
    let current: State = sqlx::query_as(
        "select limited, resets_at, windows, observed_at from credential_quotas where credential_id = $1 \
         for update",
    )
    .bind(credential)
    .fetch_one(&mut *tx)
    .await?;
    if current.limited_at(now) {
        return Ok(false);
    }
    sqlx::query(
        "update credential_quotas set limited = true, resets_at = $2 where credential_id = $1",
    )
    .bind(credential)
    .bind(now + PROBE_AFTER)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Tells whoever follows quotas that a credential's changed.
pub async fn announce(state: &AppState, credential: &CredentialRow) {
    if let Ok(Some(quota)) = for_credential(&state.db, credential.id, Utc::now()).await {
        state
            .quotas
            .send(&credential.owner, credential.id, Some(quota));
    }
}

async fn record(state: &AppState, credential: Uuid, owner: &str, observation: Observation) {
    if let Err(e) = store(&state.db, credential, &observation).await {
        tracing::warn!(%credential, error = %e, "keeping a credential's quota");
        return;
    }
    if let Ok(Some(quota)) = for_credential(&state.db, credential, Utc::now()).await {
        state.quotas.send(owner, credential, Some(quota));
    }
}

/// A `usage_update` from a session's agent: Claude Code's carry the rate limits of the Claude subscription the
/// session used: `attached`, the one last attached to its requests when it came (ADR 29, #252), and `at`, when it
/// came, are both taken before anything here waits.
pub async fn seen(
    state: &AppState,
    session: Uuid,
    update: Value,
    at: DateTime<Utc>,
    attached: Option<Uuid>,
) {
    let Some(observation) = from_claude(&update["_meta"]["_claude/rateLimit"], at) else {
        return;
    };
    let session = match db::get_session(&state.db, session).await {
        Ok(Some(row)) => row.context(),
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(%session, error = %e, "finding the Claude credential a session used");
            return;
        }
    };
    match state
        .credentials
        .used_as_of(&session, CLAUDE, attached)
        .await
    {
        Ok(Some(credential)) => record(state, credential.id, &credential.owner, observation).await,
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(session = %session.session_id, error = %e, "finding the Claude credential a session used")
        }
    }
}

/// Reads every ChatGPT login's limits, whichever sessions use it.
pub async fn read_chatgpt(state: &AppState) {
    let logins = match state.credentials.of_kind(CHATGPT).await {
        Ok(logins) => logins,
        Err(e) => {
            tracing::warn!(error = %e, "listing ChatGPT logins");
            return;
        }
    };
    for login in logins {
        read_chatgpt_login(state, &login).await;
    }
}

/// Reads one ChatGPT login's limits, e.g. one just added.
pub async fn read_chatgpt_login(state: &AppState, login: &CredentialRow) {
    if let Err(e) = read_chatgpt_of(state, login).await {
        tracing::warn!(owner = %login.owner, credential = %login.id, error = %e, "reading a ChatGPT login's limits");
    }
}

async fn read_chatgpt_of(state: &AppState, login: &CredentialRow) -> anyhow::Result<()> {
    let Some(tokens) = state.credentials.chatgpt(login).await? else {
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
    match from_codex(&usage, Utc::now()) {
        Ok(Some(observation)) => record(state, login.id, &login.owner, observation).await,
        Ok(None) => {}
        Err(e) => anyhow::bail!("ChatGPT's report wasn't as expected, so it's ignored: {e}"),
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

    fn quota(name: &str, utilization: f64) -> Quota {
        Quota {
            credential_id: Uuid::nil(),
            kind: CLAUDE.into(),
            name: name.into(),
            limited: false,
            resets_at: None,
            windows: vec![QuotaWindow {
                window: "five_hour".into(),
                utilization: Some(utilization),
                resets_at: Some("2026-11-03T12:00:00Z".into()),
                status: QuotaStatus::Allowed,
            }],
            observed_at: "2026-11-03T10:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn a_quota_change_reaches_its_owner_s_connections_and_no_one_else_s() {
        let quotas = Quotas::new().unwrap();
        let (out, mut ours) = tokio::sync::mpsc::channel(8);
        let forwarding = tokio::spawn(forward("dev", quotas.subscribe(), out, || {}));
        let gone = Uuid::from_u128(7);
        quotas.send("someone-else", Uuid::nil(), Some(quota("theirs", 0.9)));
        quotas.send("dev", Uuid::nil(), Some(quota("work", 0.4)));
        quotas.send("dev", gone, None);

        let first = ours.recv().await.unwrap();
        assert_eq!(first["method"], "_sloppod/quota");
        assert_eq!(first["params"]["quota"]["name"], "work");
        assert_eq!(first["params"]["quota"]["windows"][0]["utilization"], 0.4);
        // A credential that's gone takes its quota with it.
        let second = ours.recv().await.unwrap();
        assert_eq!(
            second["params"],
            serde_json::json!({"credentialId": gone, "quota": null})
        );
        assert!(ours.try_recv().is_err(), "nothing of anyone else's");

        // The connection ending ends it.
        drop(ours);
        quotas.send("dev", Uuid::nil(), Some(quota("work", 0.5)));
        forwarding.await.unwrap();
    }

    #[tokio::test]
    async fn a_connection_that_falls_behind_is_told_to_reconnect() {
        let quotas = Quotas::new().unwrap();
        let changes = quotas.subscribe();
        for i in 0..100 {
            quotas.send(
                "dev",
                Uuid::nil(),
                Some(quota("work", f64::from(i) / 100.0)),
            );
        }
        let (out, _ours) = tokio::sync::mpsc::channel(256);
        let lagged = std::sync::atomic::AtomicBool::new(false);
        forward("dev", changes, out, || {
            lagged.store(true, std::sync::atomic::Ordering::SeqCst)
        })
        .await;
        assert!(lagged.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn a_reset_that_passed_before_a_change_woke_it_is_still_announced() {
        // Account A's window resets at noon; a report for account B wakes the task half a second later, inside the
        // second it would have slept past the reset.
        let a = ("dev".to_owned(), quota("a", 0.9));
        let mut b = ("dev".to_owned(), quota("b", 0.1));
        b.1.credential_id = Uuid::from_u128(2);
        b.1.windows[0].resets_at = Some("2026-11-03T15:00:00Z".into());
        let planned = [a, b];
        let woken = at("2026-11-03T12:00:00.500Z");
        let due: Vec<_> = due(&planned, woken).map(|(_, q)| q.name.as_str()).collect();
        assert_eq!(due, ["a"]);
        assert_eq!(super::due(&planned, at("2026-11-03T11:59:59Z")).count(), 0);
    }

    #[test]
    fn the_next_reset_is_the_earliest_still_to_come() {
        let mut limited = quota("work", 1.0);
        assert_eq!(next_reset(&limited), Some(at("2026-11-03T12:00:00Z")));
        limited.resets_at = Some("2026-11-03T11:00:00Z".into());
        assert_eq!(next_reset(&limited), Some(at("2026-11-03T11:00:00Z")));
        limited.resets_at = None;
        limited.windows[0].resets_at = None;
        assert_eq!(next_reset(&limited), None);
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
        let observation = from_claude(&info, Utc::now()).unwrap();
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
        let observation = from_claude(&info, Utc::now()).unwrap();
        assert!(!observation.limited);
        assert_eq!(observation.resets_at, None);
        assert_eq!(
            observation.windows.keys().collect::<Vec<_>>(),
            ["seven_day"]
        );
        assert_eq!(observation.windows["seven_day"].utilization, Some(0.4));
        // A status alone says nothing of any window.
        let observation = from_claude(&json!({"status": "allowed"}), Utc::now()).unwrap();
        assert!(observation.windows.is_empty());
        assert_eq!(from_claude(&Value::Null, Utc::now()), None);
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
        let observation = from_codex(&usage, now).unwrap().unwrap();
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
            Ok(None)
        );
    }

    #[test]
    fn a_chatgpt_report_it_doesnt_recognise_is_an_error() {
        let now = at("2026-10-28T12:00:00Z");
        let window = json!({"used_percent": 10.0, "limit_window_seconds": 18_000, "reset_after_seconds": 60});
        for report in [
            json!({}),
            json!({"rate_limit": "none"}),
            json!({"rate_limit": {}}),
            json!({"rate_limit": {"allowed": true}}),
            json!({"rate_limit": {"allowed": true, "limit_reached": false, "primary_window": {"used_percent": 10.0}}}),
            json!({"rate_limit": {"allowed": true, "limit_reached": false, "primary_window": {"usedPercent": 10.0, "limit_window_seconds": 18_000}}}),
            json!({"rate_limit": {"allowed": true, "limit_reached": false, "primary_window": {"used_percent": 10.0, "limit_window_seconds": 18_000}}}),
        ] {
            assert!(from_codex(&report, now).is_err(), "{report}");
        }
        let fine = json!({"rate_limit": {"allowed": true, "limit_reached": false, "primary_window": window, "secondary_window": null}});
        assert_eq!(
            from_codex(&fine, now)
                .unwrap()
                .unwrap()
                .windows
                .keys()
                .collect::<Vec<_>>(),
            ["five_hour"]
        );
    }

    fn observation(at_: &str, limited: bool, windows: &[(&str, f64)]) -> Observation {
        Observation {
            observed_at: at(at_),
            limited,
            resets_at: None,
            windows: windows
                .iter()
                .map(|(name, utilization)| {
                    (
                        (*name).to_owned(),
                        Window {
                            utilization: Some(*utilization),
                            resets_at: None,
                            status: status_of(Some(*utilization)),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn an_older_report_never_replaces_a_newer_one() {
        let empty = State {
            limited: false,
            resets_at: None,
            windows: Json(BTreeMap::new()),
            observed_at: at("1970-01-01T00:00:00Z"),
        };
        let newer = observation("2026-10-28T12:01:00Z", true, &[("five_hour", 1.0)]);
        let older = observation(
            "2026-10-28T12:00:00Z",
            false,
            &[("five_hour", 0.9), ("seven_day", 0.5)],
        );
        // The older one arrives last: the newer refusal and window stand, and only the window it alone has is taken.
        let state = merge(merge(empty, &newer), &older);
        assert!(state.limited);
        assert_eq!(state.observed_at, newer.observed_at);
        assert_eq!(state.windows["five_hour"].window.utilization, Some(1.0));
        assert_eq!(state.windows["seven_day"].window.utilization, Some(0.5));
        assert_eq!(state.windows["seven_day"].observed_at, older.observed_at);
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
                    Kept {
                        window: Window {
                            utilization: Some(1.0),
                            resets_at: Some(at("2026-10-28T13:00:00Z")),
                            status: QuotaStatus::Rejected,
                        },
                        observed_at: at("2026-10-28T12:00:00Z"),
                    },
                ),
                (
                    "seven_day".into(),
                    Kept {
                        window: Window {
                            utilization: Some(0.7),
                            resets_at: Some(at("2026-11-01T00:00:00Z")),
                            status: QuotaStatus::Allowed,
                        },
                        observed_at: at("2026-10-28T12:00:00Z"),
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
