use chrono::{DateTime, Utc};
use serde_json::Value;
use sloppod_protocol::api::{Phase, Proposal, Session};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, FromRow, Row};
use uuid::Uuid;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect(url)
        .await?;
    MIGRATOR.run(&pool).await?;
    Ok(pool)
}

#[derive(Debug, Clone, FromRow)]
pub struct SessionRow {
    pub id: Uuid,
    pub title: String,
    pub repo: String,
    pub branch: String,
    pub phase: String,
    pub agent_session_id: Option<String>,
    pub agent_state: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl SessionRow {
    pub fn to_api(&self) -> Session {
        Session {
            id: self.id,
            title: self.title.clone(),
            repo: self.repo.clone(),
            branch: self.branch.clone(),
            phase: Phase::parse(&self.phase),
            created_at: self.created_at.to_rfc3339(),
            updated_at: self.updated_at.to_rfc3339(),
        }
    }

    pub fn phase(&self) -> Phase {
        Phase::parse(&self.phase)
    }
}

const SESSION_COLUMNS: &str =
    "id, title, repo, branch, phase, agent_session_id, agent_state, created_at, updated_at";

pub async fn insert_session(
    db: &PgPool,
    id: Uuid,
    title: &str,
    repo: &str,
    branch: &str,
    pending_prompt: Option<&str>,
) -> sqlx::Result<SessionRow> {
    sqlx::query_as(AssertSqlSafe(format!(
        "insert into sessions (id, title, repo, branch, pending_prompt) values ($1, $2, $3, $4, $5) returning {SESSION_COLUMNS}"
    )))
    .bind(id)
    .bind(title)
    .bind(repo)
    .bind(branch)
    .bind(pending_prompt)
    .fetch_one(db)
    .await
}

pub async fn list_sessions(db: &PgPool) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions order by updated_at desc"
    )))
    .fetch_all(db)
    .await
}

pub async fn active_sessions(db: &PgPool) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions where phase <> 'stopped'"
    )))
    .fetch_all(db)
    .await
}

pub async fn get_session(db: &PgPool, id: Uuid) -> sqlx::Result<Option<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions where id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await
}

pub async fn set_phase(db: &PgPool, id: Uuid, phase: Phase) -> sqlx::Result<Option<SessionRow>> {
    let stopped = phase == Phase::Stopped;
    sqlx::query_as(AssertSqlSafe(format!(
        "update sessions set phase = $2, updated_at = now(), stopped_at = case when $3 then now() else stopped_at end \
         where id = $1 and (phase <> 'stopped' or $3) returning {SESSION_COLUMNS}"
    )))
    .bind(id)
    .bind(phase.as_str())
    .bind(stopped)
    .fetch_optional(db)
    .await
}

pub async fn set_title(db: &PgPool, id: Uuid, title: &str) -> sqlx::Result<()> {
    sqlx::query("update sessions set title = $2, updated_at = now() where id = $1")
        .bind(id)
        .bind(title)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn set_agent_session(
    db: &PgPool,
    id: Uuid,
    agent_session_id: &str,
    agent_state: &Value,
) -> sqlx::Result<()> {
    sqlx::query("update sessions set agent_session_id = $2, agent_state = $3 where id = $1")
        .bind(id)
        .bind(agent_session_id)
        .bind(agent_state)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn take_pending_prompt(db: &PgPool, id: Uuid) -> sqlx::Result<Option<String>> {
    let row = sqlx::query(
        "with old as (select pending_prompt from sessions where id = $1 for update) \
         update sessions set pending_prompt = null where id = $1 returning (select pending_prompt from old)",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row.and_then(|r| r.get::<Option<String>, _>(0)))
}

pub async fn append_event(db: &PgPool, id: Uuid, payload: &Value) -> sqlx::Result<i64> {
    let seq: i64 = sqlx::query(
        "insert into session_events (session_id, payload) values ($1, $2) returning seq",
    )
    .bind(id)
    .bind(payload)
    .fetch_one(db)
    .await?
    .get(0);
    sqlx::query("update sessions set updated_at = now() where id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(seq)
}

pub async fn events(db: &PgPool, id: Uuid) -> sqlx::Result<Vec<(i64, Value)>> {
    let rows =
        sqlx::query("select seq, payload from session_events where session_id = $1 order by seq")
            .bind(id)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
}

pub async fn upsert_transcript(
    db: &PgPool,
    id: Uuid,
    name: &str,
    content: &[u8],
) -> sqlx::Result<()> {
    sqlx::query(
        "insert into transcripts (session_id, name, content) values ($1, $2, $3) \
         on conflict (session_id, name) do update set content = excluded.content, updated_at = now()",
    )
    .bind(id)
    .bind(name)
    .bind(content)
    .execute(db)
    .await
    .map(|_| ())
}

pub async fn transcripts(db: &PgPool, id: Uuid) -> sqlx::Result<Vec<(String, Vec<u8>)>> {
    let rows = sqlx::query("select name, content from transcripts where session_id = $1")
        .bind(id)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|r| (r.get(0), r.get(1))).collect())
}

#[derive(Debug, Clone, FromRow)]
pub struct ProposalRow {
    pub id: Uuid,
    pub session_id: Uuid,
    pub kind: String,
    pub status: String,
    pub payload: Value,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
}

impl ProposalRow {
    pub fn to_api(&self) -> Proposal {
        Proposal {
            id: self.id,
            session_id: self.session_id,
            kind: self.kind.clone(),
            status: self.status.clone(),
            payload: self.payload.clone(),
            result: self.result.clone(),
            error: self.error.clone(),
            reason: self.reason.clone(),
            created_at: self.created_at.to_rfc3339(),
            decided_at: self.decided_at.map(|d| d.to_rfc3339()),
        }
    }
}

const PROPOSAL_COLUMNS: &str =
    "id, session_id, kind, status, payload, result, error, reason, created_at, decided_at";

pub async fn insert_proposal(
    db: &PgPool,
    session_id: Uuid,
    kind: &str,
    payload: &Value,
) -> sqlx::Result<ProposalRow> {
    sqlx::query_as(AssertSqlSafe(format!(
        "insert into proposals (id, session_id, kind, status, payload) values ($1, $2, $3, 'pending', $4) returning {PROPOSAL_COLUMNS}"
    )))
    .bind(Uuid::now_v7())
    .bind(session_id)
    .bind(kind)
    .bind(payload)
    .fetch_one(db)
    .await
}

pub async fn get_proposal(db: &PgPool, id: Uuid) -> sqlx::Result<Option<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {PROPOSAL_COLUMNS} from proposals where id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await
}

pub async fn list_proposals(
    db: &PgPool,
    session: Option<Uuid>,
    status: Option<&str>,
) -> sqlx::Result<Vec<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {PROPOSAL_COLUMNS} from proposals where ($1::uuid is null or session_id = $1) \
         and ($2::text is null or status = $2) order by created_at"
    )))
    .bind(session)
    .bind(status)
    .fetch_all(db)
    .await
}

/// Moves a pending proposal to a decided state; returns `None` if it was no longer pending.
pub async fn decide_proposal(
    db: &PgPool,
    id: Uuid,
    status: &str,
    payload: &Value,
    reason: Option<&str>,
) -> sqlx::Result<Option<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update proposals set status = $2, payload = $3, reason = $4, decided_at = now() \
         where id = $1 and status = 'pending' returning {PROPOSAL_COLUMNS}"
    )))
    .bind(id)
    .bind(status)
    .bind(payload)
    .bind(reason)
    .fetch_optional(db)
    .await
}

pub async fn finish_proposal(
    db: &PgPool,
    id: Uuid,
    status: &str,
    result: Option<&Value>,
    error: Option<&str>,
) -> sqlx::Result<ProposalRow> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update proposals set status = $2, result = $3, error = $4 where id = $1 returning {PROPOSAL_COLUMNS}"
    )))
    .bind(id)
    .bind(status)
    .bind(result)
    .bind(error)
    .fetch_one(db)
    .await
}
