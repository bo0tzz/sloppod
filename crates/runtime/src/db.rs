use chrono::{DateTime, Utc};
use serde_json::Value;
use sloppod_contracts::{Grant, SessionContext};
use sloppod_protocol::api::{self, Phase, Proposal, Session};
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
    pub owner: String,
    pub title: String,
    pub phase: String,
    /// The agent the session runs, e.g. `claude-code`.
    pub agent: String,
    /// `work`, or `review` for a session that reviews a proposal (ADR 24).
    pub kind: String,
    /// Whether its proposals are reviewed by another model first: `off` or `required` (ADR 24).
    pub review: String,
    pub agent_session_id: Option<String>,
    pub agent_state: Option<Value>,
    /// The session's current model, used whenever its agent starts.
    pub model: Option<String>,
    /// The repository the session is about, checked out as its working directory (ADR 1).
    pub repo: Option<String>,
    grants: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When an event was last recorded; the session list is ordered by it.
    pub active_at: DateTime<Utc>,
    /// When it was deleted; it's restorable until it's purged.
    pub deleted_at: Option<DateTime<Utc>>,
}

impl SessionRow {
    /// A fresh session as the tests need it.
    #[cfg(test)]
    pub fn for_test(id: Uuid, owner: &str) -> Self {
        Self {
            id,
            owner: owner.to_owned(),
            title: String::new(),
            phase: "starting".to_owned(),
            agent: "claude-code".to_owned(),
            kind: "work".to_owned(),
            review: "off".to_owned(),
            agent_session_id: None,
            agent_state: None,
            model: None,
            repo: None,
            grants: Value::Array(Vec::new()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            active_at: Utc::now(),
            deleted_at: None,
        }
    }

    pub fn grants(&self) -> Vec<Grant> {
        serde_json::from_value(self.grants.clone()).unwrap_or_default()
    }

    pub fn context(&self) -> SessionContext {
        SessionContext {
            session_id: self.id.to_string(),
            owner: self.owner.clone(),
            grants: self.grants(),
        }
    }

    pub fn to_api(&self) -> Session {
        let context = self.context();
        let repo = match &self.repo {
            Some(repo) => context.writable_repo_for(repo),
            None => context.writable_repo(),
        };
        Session {
            id: self.id,
            title: self.title.clone(),
            repo: repo
                .as_ref()
                .map(|r| r.repo.to_owned())
                .or(self.repo.clone()),
            branch: repo.as_ref().map(|r| r.branch.to_owned()),
            grants: context
                .grants
                .iter()
                .map(|g| api::Grant {
                    capability: g.capability.clone(),
                    params: g.params.clone(),
                    description: Some(crate::grants::describe(g)),
                })
                .collect(),
            phase: Phase::parse(&self.phase),
            agent: self.agent.clone(),
            kind: self.kind.clone(),
            review: self.review.clone(),
            created_at: self.created_at.to_rfc3339(),
            updated_at: self.updated_at.to_rfc3339(),
            active_at: self.active_at.to_rfc3339(),
            deleted_at: self.deleted_at.map(|d| d.to_rfc3339()),
        }
    }

    pub fn phase(&self) -> Phase {
        Phase::parse(&self.phase)
    }
}

const SESSION_COLUMNS: &str = "id, owner, title, phase, agent, kind, review, agent_session_id, agent_state, model, repo, created_at, updated_at, active_at, deleted_at, \
     coalesce((select jsonb_agg(jsonb_build_object('capability', g.capability, 'params', g.params) order by g.created_at) \
     from session_grants g where g.session_id = sessions.id), '[]'::jsonb) as grants";

pub struct NewSessionRow<'a> {
    pub id: Uuid,
    pub owner: &'a str,
    pub title: &'a str,
    pub custom_title: bool,
    pub pending_prompt: &'a str,
    pub agent: &'a str,
    pub kind: &'a str,
    pub review: &'a str,
    pub model: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub grants: &'a [Grant],
}

pub async fn insert_session(db: &PgPool, new: NewSessionRow<'_>) -> sqlx::Result<SessionRow> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "insert into sessions (id, owner, title, custom_title, pending_prompt, model, repo, agent, kind, review) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(new.id)
    .bind(new.owner)
    .bind(new.title)
    .bind(new.custom_title)
    .bind(new.pending_prompt)
    .bind(new.model)
    .bind(new.repo)
    .bind(new.agent)
    .bind(new.kind)
    .bind(new.review)
    .execute(&mut *tx)
    .await?;
    for grant in new.grants {
        sqlx::query(
            "insert into session_grants (session_id, capability, params) values ($1, $2, $3)",
        )
        .bind(new.id)
        .bind(&grant.capability)
        .bind(&grant.params)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(get_session(db, new.id)
        .await?
        .expect("session just inserted"))
}

pub async fn list_sessions(db: &PgPool, owner: &str) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions where owner = $1 and kind = 'work' and deleted_at is null \
         order by active_at desc"
    )))
    .bind(owner)
    .fetch_all(db)
    .await
}

/// The owner's sessions that are deleted but not purged yet, most recently deleted first.
pub async fn deleted_sessions(db: &PgPool, owner: &str) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions where owner = $1 and kind = 'work' and deleted_at is not null \
         and purge_started_at is null order by deleted_at desc"
    )))
    .bind(owner)
    .fetch_all(db)
    .await
}

/// Marks a session deleted.
pub async fn mark_deleted(db: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("update sessions set deleted_at = now() where id = $1 and deleted_at is null")
        .bind(id)
        .execute(db)
        .await
        .map(|_| ())
}

/// Restores a deleted session, unless a purge has claimed it; returns whether it did.
pub async fn restore(db: &PgPool, id: Uuid) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "update sessions set deleted_at = null where id = $1 and deleted_at is not null and purge_started_at is null",
    )
    .bind(id)
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Claims the sessions deleted longer than `age` ago for purging, including ones an earlier purge didn't finish.
/// A claimed session can no longer be restored.
pub async fn claim_purges(db: &PgPool, age: chrono::Duration) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update sessions set purge_started_at = coalesce(purge_started_at, now()) \
         where deleted_at < $1 and purged_at is null returning {SESSION_COLUMNS}"
    )))
    .bind(Utc::now() - age)
    .fetch_all(db)
    .await
}

/// Records that a purge removed everything of the session.
pub async fn finish_purge(db: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("update sessions set purged_at = now() where id = $1")
        .bind(id)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn active_sessions(db: &PgPool) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions where phase not in ('stopped', 'parked')"
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

/// A session, if it belongs to `owner`.
pub async fn owned_session(db: &PgPool, id: Uuid, owner: &str) -> sqlx::Result<Option<SessionRow>> {
    Ok(get_session(db, id).await?.filter(|s| s.owner == owner))
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

/// Sets the agent's automatic title, unless the person chose one; returns whether it changed.
pub async fn set_auto_title(db: &PgPool, id: Uuid, title: &str) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "update sessions set title = $2, updated_at = now() where id = $1 and not custom_title and title <> $2",
    )
    .bind(id)
    .bind(title)
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
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
    // `old` reads the row from the statement's snapshot, so it still holds the prompt being cleared.
    let row = sqlx::query(
        "update sessions s set pending_prompt = null from sessions old \
         where s.id = $1 and old.id = s.id and old.pending_prompt is not null returning old.pending_prompt",
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
    sqlx::query("update sessions set updated_at = now(), active_at = now() where id = $1")
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

/// Appends to a transcript if the stored copy is exactly `offset` bytes long; returns whether it was.
pub async fn append_transcript(
    db: &PgPool,
    id: Uuid,
    name: &str,
    offset: u64,
    content: &[u8],
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "update transcripts set content = content || $4, updated_at = now() \
         where session_id = $1 and name = $2 and length(content) = $3",
    )
    .bind(id)
    .bind(name)
    .bind(offset as i64)
    .bind(content)
    .execute(db)
    .await?;
    Ok(result.rows_affected() == 1)
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
    pub decided_by: Option<String>,
    /// The session reviewing it, while it's in review (ADR 24).
    pub review_session_id: Option<Uuid>,
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
            decided_by: self.decided_by.clone(),
            review_session_id: self.review_session_id,
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct StandingApprovalRow {
    pub id: Uuid,
    pub kind: String,
    pub target: Option<String>,
    pub session_id: Option<Uuid>,
    pub edits: Value,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl StandingApprovalRow {
    pub fn to_api(&self) -> sloppod_protocol::api::StandingApproval {
        sloppod_protocol::api::StandingApproval {
            id: self.id,
            kind: self.kind.clone(),
            target: self.target.clone(),
            session_id: self.session_id,
            edits: self.edits.clone(),
            expires_at: self.expires_at.map(|d| d.to_rfc3339()),
            created_at: self.created_at.to_rfc3339(),
        }
    }
}

const STANDING_APPROVAL_COLUMNS: &str =
    "id, kind, target, session_id, edits, expires_at, created_at";

/// The owner's standing approvals that are neither revoked nor expired.
pub async fn standing_approvals(
    db: &PgPool,
    owner: &str,
) -> sqlx::Result<Vec<StandingApprovalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {STANDING_APPROVAL_COLUMNS} from standing_approvals \
         where owner = $1 and revoked_at is null and (expires_at is null or expires_at > now()) order by created_at"
    )))
    .bind(owner)
    .fetch_all(db)
    .await
}

pub async fn insert_standing_approval(
    db: &PgPool,
    owner: &str,
    kind: &str,
    target: Option<&str>,
    session_id: Option<Uuid>,
    edits: &Value,
    expires_at: Option<DateTime<Utc>>,
) -> sqlx::Result<StandingApprovalRow> {
    sqlx::query_as(AssertSqlSafe(format!(
        "insert into standing_approvals (id, owner, kind, target, session_id, edits, expires_at) \
         values ($1, $2, $3, $4, $5, $6, $7) returning {STANDING_APPROVAL_COLUMNS}"
    )))
    .bind(Uuid::now_v7())
    .bind(owner)
    .bind(kind)
    .bind(target)
    .bind(session_id)
    .bind(edits)
    .bind(expires_at)
    .fetch_one(db)
    .await
}

/// Revokes one of the owner's standing approvals; returns whether there was one to revoke.
pub async fn revoke_standing_approval(db: &PgPool, owner: &str, id: Uuid) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "update standing_approvals set revoked_at = now() where id = $1 and owner = $2 and revoked_at is null",
    )
    .bind(id)
    .bind(owner)
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// The oldest live standing approval of `owner` that covers a proposal of `kind` on `target` from `session_id`.
pub async fn matching_standing_approval(
    db: &PgPool,
    owner: &str,
    kind: &str,
    target: Option<&str>,
    session_id: Uuid,
) -> sqlx::Result<Option<StandingApprovalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {STANDING_APPROVAL_COLUMNS} from standing_approvals \
         where owner = $1 and kind = $2 and revoked_at is null and (expires_at is null or expires_at > now()) \
         and (target is null or lower(target) = lower($3)) and (session_id is null or session_id = $4) \
         order by created_at limit 1"
    )))
    .bind(owner)
    .bind(kind)
    .bind(target)
    .bind(session_id)
    .fetch_optional(db)
    .await
}

const PROPOSAL_COLUMNS: &str = "id, session_id, kind, status, payload, result, error, reason, created_at, decided_at, decided_by, review_session_id";

/// A new proposal, `pending` a human's review or `in_review` by a review session first (ADR 24).
pub async fn insert_proposal(
    db: &PgPool,
    session_id: Uuid,
    kind: &str,
    status: &str,
    payload: &Value,
) -> sqlx::Result<ProposalRow> {
    sqlx::query_as(AssertSqlSafe(format!(
        "insert into proposals (id, session_id, kind, status, payload) values ($1, $2, $3, $4, $5) returning {PROPOSAL_COLUMNS}"
    )))
    .bind(Uuid::now_v7())
    .bind(session_id)
    .bind(kind)
    .bind(status)
    .bind(payload)
    .fetch_one(db)
    .await
}

/// Proposals that were approved but not carried out, e.g. because the runtime restarted in between.
pub async fn approved_proposals(db: &PgPool) -> sqlx::Result<Vec<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {PROPOSAL_COLUMNS} from proposals where status = 'approved' order by decided_at"
    )))
    .fetch_all(db)
    .await
}

/// Withdraws one of the session's pending proposals; `None` if it isn't one.
pub async fn withdraw_proposal(
    db: &PgPool,
    id: Uuid,
    session_id: Uuid,
    reason: Option<&str>,
) -> sqlx::Result<Option<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update proposals set status = 'withdrawn', reason = $3, decided_by = 'agent', decided_at = now() \
         where id = $1 and session_id = $2 and status in ('pending', 'in_review', 'changes_requested') \
         returning {PROPOSAL_COLUMNS}"
    )))
    .bind(id)
    .bind(session_id)
    .bind(reason)
    .fetch_optional(db)
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
    owner: &str,
    session: Option<Uuid>,
    status: Option<&str>,
) -> sqlx::Result<Vec<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {PROPOSAL_COLUMNS} from proposals where session_id in (select id from sessions where owner = $1) \
         and ($2::uuid is null or session_id = $2) and ($3::text is null or status = $3) order by created_at"
    )))
    .bind(owner)
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
    decided_by: &str,
) -> sqlx::Result<Option<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update proposals set status = $2, payload = $3, reason = $4, decided_by = $5, decided_at = now() \
         where id = $1 and status = 'pending' returning {PROPOSAL_COLUMNS}"
    )))
    .bind(id)
    .bind(status)
    .bind(payload)
    .bind(reason)
    .bind(decided_by)
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

pub async fn add_grant(db: &PgPool, session: Uuid, grant: &Grant) -> sqlx::Result<()> {
    sqlx::query("insert into session_grants (session_id, capability, params) values ($1, $2, $3)")
        .bind(session)
        .bind(&grant.capability)
        .bind(&grant.params)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn set_session_model(db: &PgPool, id: Uuid, model: &str) -> sqlx::Result<()> {
    sqlx::query("update sessions set model = $2 where id = $1")
        .bind(id)
        .bind(model)
        .execute(db)
        .await
        .map(|_| ())
}

pub async fn set_session_review(db: &PgPool, id: Uuid, review: &str) -> sqlx::Result<()> {
    sqlx::query("update sessions set review = $2 where id = $1")
        .bind(id)
        .bind(review)
        .execute(db)
        .await
        .map(|_| ())
}

/// The owner's review policy for new sessions (ADR 24).
pub async fn user_review(db: &PgPool, owner: &str) -> sqlx::Result<String> {
    let row: Option<(String,)> =
        sqlx::query_as("select review from user_settings where owner = $1")
            .bind(owner)
            .fetch_optional(db)
            .await?;
    Ok(row.map_or_else(|| "off".to_owned(), |(review,)| review))
}

pub async fn set_user_review(db: &PgPool, owner: &str, review: &str) -> sqlx::Result<()> {
    sqlx::query(
        "insert into user_settings (owner, review) values ($1, $2) \
         on conflict (owner) do update set review = excluded.review, updated_at = now()",
    )
    .bind(owner)
    .bind(review)
    .execute(db)
    .await
    .map(|_| ())
}

pub async fn user_model(db: &PgPool, owner: &str) -> sqlx::Result<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("select model from user_settings where owner = $1")
            .bind(owner)
            .fetch_optional(db)
            .await?;
    Ok(row.and_then(|(model,)| model))
}

pub async fn set_user_model(db: &PgPool, owner: &str, model: Option<&str>) -> sqlx::Result<()> {
    sqlx::query(
        "insert into user_settings (owner, model) values ($1, $2) \
         on conflict (owner) do update set model = excluded.model, updated_at = now()",
    )
    .bind(owner)
    .bind(model)
    .execute(db)
    .await
    .map(|_| ())
}

/// The agent's state from the owner's most recently active session, for the models it offered.
pub async fn latest_agent_state(db: &PgPool, owner: &str) -> sqlx::Result<Option<Value>> {
    let row: Option<(Option<Value>,)> = sqlx::query_as(
        "select agent_state from sessions where owner = $1 and agent_state is not null order by updated_at desc limit 1",
    )
    .bind(owner)
    .fetch_optional(db)
    .await?;
    Ok(row.and_then(|(state,)| state))
}

/// Moves a proposal from `from` to `to`, replacing its payload when given; `None` if it wasn't in `from`.
pub async fn move_proposal(
    db: &PgPool,
    id: Uuid,
    from: &str,
    to: &str,
    payload: Option<&Value>,
) -> sqlx::Result<Option<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update proposals set status = $3, payload = coalesce($4, payload) where id = $1 and status = $2 \
         returning {PROPOSAL_COLUMNS}"
    )))
    .bind(id)
    .bind(from)
    .bind(to)
    .bind(payload)
    .fetch_optional(db)
    .await
}

pub async fn set_review_session(db: &PgPool, proposal: Uuid, session: Uuid) -> sqlx::Result<()> {
    sqlx::query("update proposals set review_session_id = $2 where id = $1")
        .bind(proposal)
        .bind(session)
        .execute(db)
        .await?;
    Ok(())
}

/// Points a session's grant of `repo` at `commit`, which the session checks out at its next start.
pub async fn set_repo_commit(
    db: &PgPool,
    session: Uuid,
    repo: &str,
    commit: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "update session_grants set params = jsonb_set(params, '{commit}', to_jsonb($3::text)) \
         where session_id = $1 and capability = 'repo' and lower(params->>'repo') = lower($2)",
    )
    .bind(session)
    .bind(repo)
    .bind(commit)
    .execute(db)
    .await?;
    Ok(())
}

/// A session's proposals that are still with their reviewer: in review, or waiting on the author's fixes.
pub async fn proposals_in_review(db: &PgPool, session: Uuid) -> sqlx::Result<Vec<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {PROPOSAL_COLUMNS} from proposals where session_id = $1 and status in ('in_review', 'changes_requested')"
    )))
    .bind(session)
    .fetch_all(db)
    .await
}

/// The proposal a review session reviews.
pub async fn reviewed_proposal(db: &PgPool, session: Uuid) -> sqlx::Result<Option<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {PROPOSAL_COLUMNS} from proposals where review_session_id = $1"
    )))
    .bind(session)
    .fetch_optional(db)
    .await
}

#[derive(Debug, Clone, FromRow)]
pub struct ReviewRow {
    pub round: i32,
    pub head_sha: String,
    pub replies: Option<String>,
    pub verdict: Option<String>,
    pub summary: Option<String>,
    pub findings: Value,
    pub resolved: Value,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl ReviewRow {
    pub fn to_api(&self) -> api::Review {
        api::Review {
            round: self.round,
            head_sha: self.head_sha.clone(),
            replies: self.replies.clone(),
            verdict: self.verdict.clone(),
            summary: self.summary.clone(),
            findings: self.findings.clone(),
            resolved: self.resolved.clone(),
            created_at: self.created_at.to_rfc3339(),
            finished_at: self.finished_at.map(|d| d.to_rfc3339()),
        }
    }
}

const REVIEW_COLUMNS: &str =
    "round, head_sha, replies, verdict, summary, findings, resolved, created_at, finished_at";

/// A proposal's review rounds, first to last.
pub async fn reviews(db: &PgPool, proposal: Uuid) -> sqlx::Result<Vec<ReviewRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {REVIEW_COLUMNS} from reviews where proposal_id = $1 order by round"
    )))
    .bind(proposal)
    .fetch_all(db)
    .await
}

/// Starts the next review round of a proposal; returns its number.
pub async fn start_review_round(
    db: &PgPool,
    proposal: Uuid,
    head_sha: &str,
    replies: Option<&str>,
) -> sqlx::Result<i32> {
    sqlx::query_scalar(
        "insert into reviews (proposal_id, round, head_sha, replies) \
         values ($1, coalesce((select max(round) from reviews where proposal_id = $1), 0) + 1, $2, $3) \
         returning round",
    )
    .bind(proposal)
    .bind(head_sha)
    .bind(replies)
    .fetch_one(db)
    .await
}

/// Records the verdict of a proposal's running review round; `None` if no round is running.
pub async fn finish_review_round(
    db: &PgPool,
    proposal: Uuid,
    verdict: &str,
    summary: &str,
    findings: &Value,
    resolved: &Value,
) -> sqlx::Result<Option<ReviewRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update reviews set verdict = $2, summary = $3, findings = $4, resolved = $5, finished_at = now() \
         where proposal_id = $1 and verdict is null returning {REVIEW_COLUMNS}"
    )))
    .bind(proposal)
    .bind(verdict)
    .bind(summary)
    .bind(findings)
    .bind(resolved)
    .fetch_optional(db)
    .await
}
