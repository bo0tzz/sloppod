use chrono::{DateTime, Utc};
use serde_json::Value;
use sloppod_contracts::{Grant, SessionContext};
use sloppod_protocol::api::{
    self, Phase, Proposal, ProposalStatus, ReviewPolicy, Session, SessionKind,
};
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
    #[sqlx(try_from = "String")]
    pub kind: SessionKind,
    #[sqlx(try_from = "String")]
    pub review: ReviewPolicy,
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
    pub agent_status: Option<String>,
    pub agent_error: Option<String>,
    /// The session that spawned this one, for a sub-session (ADR 27).
    pub parent_id: Option<Uuid>,
    /// When the session, which hit its usage limit, carries on by itself (#251).
    pub limited_until: Option<DateTime<Utc>>,
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
            kind: SessionKind::Work,
            review: ReviewPolicy::Off,
            agent_session_id: None,
            agent_state: None,
            model: None,
            repo: None,
            grants: Value::Array(Vec::new()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            active_at: Utc::now(),
            deleted_at: None,
            agent_status: None,
            agent_error: None,
            parent_id: None,
            limited_until: None,
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
            kind: self.kind,
            review: self.review,
            created_at: self.created_at.to_rfc3339(),
            updated_at: self.updated_at.to_rfc3339(),
            active_at: self.active_at.to_rfc3339(),
            deleted_at: self.deleted_at.map(|d| d.to_rfc3339()),
            agent_status: self.agent_status.clone(),
            agent_error: self.agent_error.clone(),
            parent_id: self.parent_id,
            limited_until: self.limited_until.map(|d| d.to_rfc3339()),
        }
    }

    pub fn phase(&self) -> Phase {
        Phase::parse(&self.phase)
    }

    /// Why the session can't act any more, if it can't: it's stopped, parked or deleted. A pod of it that's still up
    /// gets nothing done in its name: no egress through the authorizer, no tools, no memory (#60).
    pub fn not_acting(&self) -> Option<&'static str> {
        if self.deleted_at.is_some() {
            return Some("deleted");
        }
        match self.phase() {
            Phase::Stopped => Some("stopped"),
            Phase::Parked => Some("parked"),
            _ => None,
        }
    }
}

const SESSION_COLUMNS: &str = "id, owner, title, phase, agent, kind, review, agent_session_id, agent_state, model, repo, created_at, updated_at, active_at, deleted_at, agent_status, agent_error, parent_id, limited_until, \
     coalesce((select jsonb_agg(jsonb_build_object('capability', g.capability, 'params', g.params) order by g.created_at) \
     from session_grants g where g.session_id = sessions.id), '[]'::jsonb) as grants";

pub struct NewSessionRow<'a> {
    pub id: Uuid,
    pub owner: &'a str,
    pub title: &'a str,
    pub custom_title: bool,
    pub pending_prompt: &'a str,
    pub agent: &'a str,
    pub kind: SessionKind,
    pub review: ReviewPolicy,
    pub model: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub grants: &'a [Grant],
}

/// Makes `child` a sub-session of `parent` (ADR 27).
pub async fn set_parent(db: &PgPool, child: Uuid, parent: Uuid) -> sqlx::Result<()> {
    sqlx::query("update sessions set parent_id = $2 where id = $1")
        .bind(child)
        .bind(parent)
        .execute(db)
        .await?;
    Ok(())
}

/// A session's sub-sessions, oldest first.
pub async fn children(db: &PgPool, parent: Uuid) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions where parent_id = $1 and deleted_at is null order by created_at"
    )))
    .bind(parent)
    .fetch_all(db)
    .await
}

/// Whether `session` or any session it's a sub-session of is deleted.
pub async fn deleted_in_line(db: &PgPool, session: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "with recursive up as (select id, parent_id, deleted_at from sessions where id = $1 \
           union all select s.id, s.parent_id, s.deleted_at from sessions s join up on s.id = up.parent_id) \
         select exists(select 1 from up where deleted_at is not null)",
    )
    .bind(session)
    .fetch_one(db)
    .await
}

/// Every sub-session below `session` that isn't deleted, at any depth, also below sub-sessions that are.
pub async fn live_descendants(db: &PgPool, session: Uuid) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "with recursive down as (select id from sessions where parent_id = $1 \
           union all select s.id from sessions s join down on s.parent_id = down.id) \
         select {SESSION_COLUMNS} from sessions where id in (select id from down) and deleted_at is null \
         order by created_at"
    )))
    .bind(session)
    .fetch_all(db)
    .await
}

/// How many sub-sessions of `session`'s tree, its root's descendants, are running: what ADR 27's limit counts.
pub async fn running_in_tree(db: &PgPool, session: Uuid) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "with recursive up as (select id, parent_id from sessions where id = $1 \
           union all select s.id, s.parent_id from sessions s join up on s.id = up.parent_id), \
         down as (select id from up where parent_id is null \
           union all select s.id from sessions s join down on s.parent_id = down.id) \
         select count(*) from sessions where id in (select id from down) and parent_id is not null \
           and deleted_at is null and phase not in ('stopped', 'parked')",
    )
    .bind(session)
    .fetch_one(db)
    .await
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
    .bind(new.kind.as_str())
    .bind(new.review.as_str())
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

/// A pull request the runtime opened for a session, and what it last saw of it (`pulls`).
#[derive(Debug, FromRow)]
pub struct WatchedPull {
    pub session_id: Uuid,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub observed: Option<Value>,
}

/// The open pull requests the runtime opened, from executed pull request and stack proposals, of sessions that
/// aren't stopped or deleted. One last seen merged or closed isn't watched any more.
pub async fn watched_pulls(db: &PgPool) -> sqlx::Result<Vec<WatchedPull>> {
    sqlx::query_as(
        "with opened as ( \
           select p.session_id, lower(p.payload->>'repo') as repo, (p.result->>'number')::bigint as number \
           from proposals p where p.kind = 'github.pr.create' and p.status = 'executed' and p.result ? 'number' \
           union \
           select p.session_id, lower(p.payload->>'repo'), (pull->>'number')::bigint \
           from proposals p, jsonb_array_elements(p.result->'pull_requests') pull \
           where p.kind = 'github.stack.create' and p.status = 'executed' and pull ? 'number' \
         ) \
         select o.session_id, s.owner, o.repo, o.number, w.observed from opened o \
         join sessions s on s.id = o.session_id \
         left join pull_watches w on w.repo = o.repo and w.number = o.number \
         where s.phase <> 'stopped' and s.deleted_at is null and coalesce(w.observed->>'state', 'OPEN') = 'OPEN' \
         order by o.repo, o.number",
    )
    .fetch_all(db)
    .await
}

/// The owner's utility session (ADR 26), if they have one that isn't stopped.
pub async fn utility_session(db: &PgPool, owner: &str) -> sqlx::Result<Option<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions where owner = $1 and kind = 'utility' and phase <> 'stopped' \
         and deleted_at is null order by created_at limit 1"
    )))
    .bind(owner)
    .fetch_optional(db)
    .await
}

/// Every owner's utility session that isn't stopped.
pub async fn utility_sessions(db: &PgPool) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions where kind = 'utility' and phase <> 'stopped' and deleted_at is null"
    )))
    .fetch_all(db)
    .await
}

/// Keeps what was last seen of a pull request.
pub async fn save_pull_watch(
    db: &PgPool,
    repo: &str,
    number: i64,
    session: Uuid,
    observed: &Value,
) -> sqlx::Result<()> {
    sqlx::query(
        "insert into pull_watches (repo, number, session_id, observed) values ($1, $2, $3, $4) \
         on conflict (repo, number) do update set observed = excluded.observed, updated_at = now()",
    )
    .bind(repo)
    .bind(number)
    .bind(session)
    .bind(observed)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn active_sessions(db: &PgPool) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions where phase not in ('stopped', 'parked')"
    )))
    .fetch_all(db)
    .await
}

pub async fn parked_sessions(db: &PgPool) -> sqlx::Result<Vec<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {SESSION_COLUMNS} from sessions where phase = 'parked'"
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

/// Says what the pod is doing while the agent starts.
pub async fn set_agent_status(db: &PgPool, id: Uuid, status: &str) -> sqlx::Result<()> {
    sqlx::query("update sessions set agent_status = $2 where id = $1")
        .bind(id)
        .bind(status)
        .execute(db)
        .await?;
    Ok(())
}

/// Records why the agent failed; returns whether that's news, rather than the failure it had last time.
pub async fn set_agent_error(db: &PgPool, id: Uuid, error: &str) -> sqlx::Result<bool> {
    let (news,): (bool,) = sqlx::query_as(
        "update sessions s set agent_status = null, agent_error = $2 from sessions old \
         where s.id = $1 and old.id = s.id returning old.agent_error is distinct from $2",
    )
    .bind(id)
    .bind(error)
    .fetch_one(db)
    .await?;
    Ok(news)
}

/// The agent is ready: what it was doing and why it failed before no longer apply.
pub async fn clear_agent_status(db: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("update sessions set agent_status = null, agent_error = null where id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
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

/// Replaces a session's kept queue (#39); an empty one is removed.
pub async fn save_queue(db: &PgPool, id: Uuid, queue: &[Value]) -> sqlx::Result<()> {
    if queue.is_empty() {
        sqlx::query("delete from session_queues where session_id = $1")
            .bind(id)
            .execute(db)
            .await?;
        return Ok(());
    }
    sqlx::query(
        "insert into session_queues (session_id, messages) values ($1, $2) \
         on conflict (session_id) do update set messages = excluded.messages, updated_at = now()",
    )
    .bind(id)
    .bind(Value::from(queue.to_vec()))
    .execute(db)
    .await
    .map(|_| ())
}

/// Sets when the session, waiting for its usage limit, carries on; none when it isn't waiting.
pub async fn set_limited_until(
    db: &PgPool,
    id: Uuid,
    until: Option<DateTime<Utc>>,
) -> sqlx::Result<Option<SessionRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update sessions set limited_until = $2 where id = $1 returning {SESSION_COLUMNS}"
    )))
    .bind(id)
    .bind(until)
    .fetch_optional(db)
    .await
}

/// Ends a session's wait for its usage limit at `at`, and in the same transaction the wait of a review round it works
/// on as a reviewer, so no restart can come between the two and count its downtime as waiting.
pub async fn end_limit(
    db: &PgPool,
    id: Uuid,
    at: DateTime<Utc>,
) -> sqlx::Result<Option<SessionRow>> {
    let mut tx = db.begin().await?;
    let row = sqlx::query_as(AssertSqlSafe(format!(
        "update sessions set limited_until = null where id = $1 returning {SESSION_COLUMNS}"
    )))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "update reviews r set paused_seconds = r.paused_seconds + greatest(0, extract(epoch from $2 - r.paused_since))::bigint, \
         paused_since = null from proposals p \
         where p.review_session_id = $1 and r.proposal_id = p.id and r.paused_since is not null",
    )
    .bind(id)
    .bind(at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// The sessions waiting for their usage limit, and when they carry on.
pub async fn limited_sessions(db: &PgPool) -> sqlx::Result<Vec<(Uuid, DateTime<Utc>)>> {
    sqlx::query_as(
        "select id, limited_until from sessions where limited_until is not null and phase <> 'stopped' and deleted_at is null",
    )
    .fetch_all(db)
    .await
}

/// Every kept queue, oldest change first.
pub async fn queues(db: &PgPool) -> sqlx::Result<Vec<(Uuid, Vec<Value>)>> {
    let rows: Vec<(Uuid, Value)> =
        sqlx::query_as("select session_id, messages from session_queues order by updated_at")
            .fetch_all(db)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, messages)| (id, messages.as_array().cloned().unwrap_or_default()))
        .collect())
}

/// The session's first message, until a turn has started with it.
pub async fn pending_prompt(db: &PgPool, id: Uuid) -> sqlx::Result<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("select pending_prompt from sessions where id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?;
    Ok(row.and_then(|(prompt,)| prompt))
}

pub async fn clear_pending_prompt(db: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("update sessions set pending_prompt = null where id = $1")
        .bind(id)
        .execute(db)
        .await
        .map(|_| ())
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

/// The sequence number of the session's latest event; 0 when it has none.
pub async fn last_event(db: &PgPool, id: Uuid) -> sqlx::Result<i64> {
    sqlx::query_scalar("select coalesce(max(seq), 0) from session_events where session_id = $1")
        .bind(id)
        .fetch_one(db)
        .await
}

/// Whether the session has a user message with this id, as its UI gave it.
pub async fn has_message(db: &PgPool, id: Uuid, message_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "select exists(select 1 from session_events where session_id = $1 \
         and payload->>'sessionUpdate' = 'user_message_chunk' and payload->>'messageId' = $2)",
    )
    .bind(id)
    .bind(message_id.to_string())
    .fetch_one(db)
    .await
}

/// Notes that the user took the message with this id back out of the session's queue.
pub async fn remove_message(db: &PgPool, id: Uuid, message_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "insert into session_removed_messages (session_id, message_id) values ($1, $2) \
         on conflict do nothing",
    )
    .bind(id)
    .bind(message_id)
    .execute(db)
    .await
    .map(|_| ())
}

/// Whether the user took the message with this id back out of the session's queue.
pub async fn message_removed(db: &PgPool, id: Uuid, message_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "select exists(select 1 from session_removed_messages where session_id = $1 and message_id = $2)",
    )
    .bind(id)
    .bind(message_id)
    .fetch_one(db)
    .await
}

/// The text of the session's first message, e.g. a sub-session's task.
pub async fn first_message(db: &PgPool, id: Uuid) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "select payload->'content'->>'text' from session_events where session_id = $1 \
         and payload->>'sessionUpdate' = 'user_message_chunk' order by seq limit 1",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .map(Option::flatten)
}

/// Up to `limit` of the session's latest events before `before`, in order.
pub async fn events_before(
    db: &PgPool,
    id: Uuid,
    before: i64,
    limit: i64,
) -> sqlx::Result<Vec<(i64, Value)>> {
    let rows = sqlx::query(
        "select seq, payload from (select seq, payload from session_events where session_id = $1 and seq < $2 \
         order by seq desc limit $3) latest order by seq",
    )
    .bind(id)
    .bind(before)
    .bind(limit)
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

/// Where a replacement of a transcript file is put together until all of it is in. It isn't a transcript file name,
/// so it's never restored to a pod.
pub fn staged_transcript(name: &str) -> String {
    format!("{name}.staged")
}

/// Makes a staged replacement the transcript file, in one step.
pub async fn promote_transcript(db: &PgPool, id: Uuid, name: &str) -> sqlx::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("delete from transcripts where session_id = $1 and name = $2")
        .bind(id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "update transcripts set name = $2, updated_at = now() where session_id = $1 and name = $3",
    )
    .bind(id)
    .bind(name)
    .bind(staged_transcript(name))
    .execute(&mut *tx)
    .await?;
    tx.commit().await
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

/// The session's transcript files and how long each is.
pub async fn transcript_lens(db: &PgPool, id: Uuid) -> sqlx::Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "select name, octet_length(content)::bigint from transcripts where session_id = $1 and name like '%.jsonl' \
         order by name",
    )
    .bind(id)
    .fetch_all(db)
    .await
}

/// How long a transcript file is, if the session has it.
pub async fn transcript_len(db: &PgPool, id: Uuid, name: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar(
        "select octet_length(content)::bigint from transcripts where session_id = $1 and name = $2",
    )
    .bind(id)
    .bind(name)
    .fetch_optional(db)
    .await
}

/// Up to `len` bytes of a transcript file from `offset` on.
pub async fn transcript_chunk(
    db: &PgPool,
    id: Uuid,
    name: &str,
    offset: i64,
    len: i64,
) -> sqlx::Result<Vec<u8>> {
    let chunk: Option<Vec<u8>> = sqlx::query_scalar(
        "select substring(content from ($3 + 1)::int for $4::int) from transcripts where session_id = $1 and name = $2",
    )
    .bind(id)
    .bind(name)
    .bind(offset)
    .bind(len)
    .fetch_optional(db)
    .await?;
    Ok(chunk.unwrap_or_default())
}

pub async fn transcripts(db: &PgPool, id: Uuid) -> sqlx::Result<Vec<(String, Vec<u8>)>> {
    let rows = sqlx::query(
        "select name, content from transcripts where session_id = $1 and name like '%.jsonl'",
    )
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
    #[sqlx(try_from = "String")]
    pub status: ProposalStatus,
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
            status: self.status,
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
    status: ProposalStatus,
    payload: &Value,
) -> sqlx::Result<ProposalRow> {
    sqlx::query_as(AssertSqlSafe(format!(
        "insert into proposals (id, session_id, kind, status, payload) values ($1, $2, $3, $4, $5) returning {PROPOSAL_COLUMNS}"
    )))
    .bind(Uuid::now_v7())
    .bind(session_id)
    .bind(kind)
    .bind(status.as_str())
    .bind(payload)
    .fetch_one(db)
    .await
}

pub async fn proposals_with_status(
    db: &PgPool,
    status: ProposalStatus,
) -> sqlx::Result<Vec<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {PROPOSAL_COLUMNS} from proposals where status = $1"
    )))
    .bind(status.as_str())
    .fetch_all(db)
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

/// Stacks that were set to merge themselves; the watch on each ends at its first check once it has merged.
/// The fingerprint of the change a pull request made when it was proposed and set to merge itself, if it was.
pub async fn pull_fingerprint(
    db: &PgPool,
    repo: &str,
    number: u64,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "select result->>'fingerprint' from proposals where kind = 'github.pr.create' and status = 'executed' \
         and lower(payload->>'repo') = lower($1) and (result->>'number')::bigint = $2 \
         and result ? 'fingerprint' order by created_at desc limit 1",
    )
    .bind(repo)
    .bind(number as i64)
    .fetch_optional(db)
    .await
}

pub async fn merging_stacks(db: &PgPool) -> sqlx::Result<Vec<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {PROPOSAL_COLUMNS} from proposals where kind = 'github.stack.create' and status = 'executed' \
         and (result->>'auto_merge')::boolean"
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
    status: Option<ProposalStatus>,
) -> sqlx::Result<Vec<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {PROPOSAL_COLUMNS} from proposals where session_id in (select id from sessions where owner = $1) \
         and ($2::uuid is null or session_id = $2) and ($3::text is null or status = $3) order by created_at"
    )))
    .bind(owner)
    .bind(session)
    .bind(status.map(ProposalStatus::as_str))
    .fetch_all(db)
    .await
}

/// Moves a pending proposal to a decided state; returns `None` if it was no longer pending.
pub async fn decide_proposal(
    db: &PgPool,
    id: Uuid,
    status: ProposalStatus,
    payload: &Value,
    reason: Option<&str>,
    decided_by: &str,
) -> sqlx::Result<Option<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update proposals set status = $2, payload = $3, reason = $4, decided_by = $5, decided_at = now() \
         where id = $1 and status = 'pending' returning {PROPOSAL_COLUMNS}"
    )))
    .bind(id)
    .bind(status.as_str())
    .bind(payload)
    .bind(reason)
    .bind(decided_by)
    .fetch_optional(db)
    .await
}

pub async fn finish_proposal(
    db: &PgPool,
    id: Uuid,
    status: ProposalStatus,
    result: Option<&Value>,
    error: Option<&str>,
) -> sqlx::Result<ProposalRow> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update proposals set status = $2, result = $3, error = $4 where id = $1 returning {PROPOSAL_COLUMNS}"
    )))
    .bind(id)
    .bind(status.as_str())
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

pub async fn set_session_review(db: &PgPool, id: Uuid, review: ReviewPolicy) -> sqlx::Result<()> {
    sqlx::query("update sessions set review = $2 where id = $1")
        .bind(id)
        .bind(review.as_str())
        .execute(db)
        .await
        .map(|_| ())
}

/// The owner's review policy for new sessions (ADR 24).
pub async fn user_review(db: &PgPool, owner: &str) -> sqlx::Result<ReviewPolicy> {
    let row: Option<(String,)> =
        sqlx::query_as("select review from user_settings where owner = $1")
            .bind(owner)
            .fetch_optional(db)
            .await?;
    row.map_or(Ok(ReviewPolicy::Off), |(review,)| {
        review
            .parse()
            .map_err(|e: String| sqlx::Error::Decode(e.into()))
    })
}

pub async fn set_user_review(db: &PgPool, owner: &str, review: ReviewPolicy) -> sqlx::Result<()> {
    sqlx::query(
        "insert into user_settings (owner, review) values ($1, $2) \
         on conflict (owner) do update set review = excluded.review, updated_at = now()",
    )
    .bind(owner)
    .bind(review.as_str())
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
    from: ProposalStatus,
    to: ProposalStatus,
    payload: Option<&Value>,
) -> sqlx::Result<Option<ProposalRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "update proposals set status = $3, payload = coalesce($4, payload) where id = $1 and status = $2 \
         returning {PROPOSAL_COLUMNS}"
    )))
    .bind(id)
    .bind(from.as_str())
    .bind(to.as_str())
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
    /// How long the reviewer has waited for its usage limit in this round, in seconds, not counting a wait still on.
    pub paused_seconds: i64,
    /// When the reviewer's wait for its usage limit that's still on started.
    pub paused_since: Option<DateTime<Utc>>,
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

const REVIEW_COLUMNS: &str = "round, head_sha, replies, verdict, summary, findings, resolved, created_at, finished_at, paused_seconds, paused_since";

/// A proposal's review rounds, first to last.
pub async fn reviews(db: &PgPool, proposal: Uuid) -> sqlx::Result<Vec<ReviewRow>> {
    sqlx::query_as(AssertSqlSafe(format!(
        "select {REVIEW_COLUMNS} from reviews where proposal_id = $1 order by round"
    )))
    .bind(proposal)
    .fetch_all(db)
    .await
}

/// Marks a review round's reviewer as waiting for its usage limit from `at`, unless it already is; kept with the
/// round, so time the runtime is down during the wait counts as waiting too.
pub async fn pause_review(
    db: &PgPool,
    proposal: Uuid,
    round: i32,
    at: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query(
        "update reviews set paused_since = $3 where proposal_id = $1 and round = $2 and paused_since is null",
    )
    .bind(proposal)
    .bind(round)
    .bind(at)
    .execute(db)
    .await
    .map(|_| ())
}

/// Marks the running review round a reviewer session works on as waiting for its usage limit from `at`, unless it
/// already is; done as the limit hits, so a restart before the round's watcher looks doesn't lose the wait.
pub async fn pause_reviewer(db: &PgPool, reviewer: Uuid, at: DateTime<Utc>) -> sqlx::Result<()> {
    sqlx::query(
        "update reviews r set paused_since = $2 from proposals p \
         where p.review_session_id = $1 and p.status = 'in_review' and r.proposal_id = p.id \
         and r.verdict is null and r.paused_since is null \
         and r.round = (select max(round) from reviews where proposal_id = p.id)",
    )
    .bind(reviewer)
    .bind(at)
    .execute(db)
    .await
    .map(|_| ())
}

/// Ends a review round's wait for the usage limit at `at`, adding it to the time the round has waited.
pub async fn unpause_review(
    db: &PgPool,
    proposal: Uuid,
    round: i32,
    at: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query(
        "update reviews set paused_seconds = paused_seconds + greatest(0, extract(epoch from $3 - paused_since))::bigint, \
         paused_since = null where proposal_id = $1 and round = $2 and paused_since is not null",
    )
    .bind(proposal)
    .bind(round)
    .bind(at)
    .execute(db)
    .await
    .map(|_| ())
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
