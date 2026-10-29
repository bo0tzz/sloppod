//! What the session hub keeps in the database, behind a trait so the hub can run against memory in tests.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sloppod_protocol::api::Phase;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{self, SessionRow};

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn get_session(&self, id: Uuid) -> sqlx::Result<Option<SessionRow>>;
    async fn set_phase(&self, id: Uuid, phase: Phase) -> sqlx::Result<Option<SessionRow>>;
    async fn set_auto_title(&self, id: Uuid, title: &str) -> sqlx::Result<bool>;
    async fn set_session_model(&self, id: Uuid, model: &str) -> sqlx::Result<()>;
    /// Appends to the session's event log and returns the event's sequence number.
    async fn append_event(&self, id: Uuid, payload: &Value) -> sqlx::Result<i64>;
    /// The sequence number of the session's latest event; 0 when it has none.
    async fn last_event(&self, id: Uuid) -> sqlx::Result<i64>;
    /// Whether the session has a user message with this id, as its UI gave it.
    async fn has_message(&self, id: Uuid, message_id: Uuid) -> sqlx::Result<bool>;
    /// Notes that the user took the message with this id back out of the session's queue.
    async fn remove_message(&self, id: Uuid, message_id: Uuid) -> sqlx::Result<()>;
    /// Whether the user took the message with this id back out of the session's queue.
    async fn message_removed(&self, id: Uuid, message_id: Uuid) -> sqlx::Result<bool>;
    /// Up to `limit` of the session's latest events before `before`, in order.
    async fn events_before(
        &self,
        id: Uuid,
        before: i64,
        limit: i64,
    ) -> sqlx::Result<Vec<(i64, Value)>>;
    /// Keeps a session's queued messages, `{id, blocks}` each, replacing what was kept; none clears them.
    async fn save_queue(&self, id: Uuid, queue: &[Value]) -> sqlx::Result<()>;
    /// Every session's kept queue, for after a restart.
    async fn queues(&self) -> sqlx::Result<Vec<(Uuid, Vec<Value>)>>;
    /// Keeps questions the session's agent asked with `ask_user`, until they're answered.
    async fn save_question(&self, id: Uuid, question: Uuid, questions: &Value) -> sqlx::Result<()>;
    /// Forgets an answered question.
    async fn forget_question(&self, question: Uuid) -> sqlx::Result<()>;
    /// Every kept question, `(session, question, questions)`, oldest first, for after a restart.
    async fn questions(&self) -> sqlx::Result<Vec<(Uuid, Uuid, Value)>>;
    /// The session's first message, until a turn has started with it.
    async fn pending_prompt(&self, id: Uuid) -> sqlx::Result<Option<String>>;
    /// Forgets the session's first message, once a turn has started with it.
    async fn clear_pending_prompt(&self, id: Uuid) -> sqlx::Result<()>;
    /// Sets when the session, waiting for its usage limit, carries on; none when it isn't waiting.
    async fn set_limited_until(
        &self,
        id: Uuid,
        until: Option<DateTime<Utc>>,
    ) -> sqlx::Result<Option<SessionRow>>;
    /// Ends the session's wait for its usage limit at `at`, with a review round's wait it works on as a reviewer.
    async fn end_limit(&self, id: Uuid, at: DateTime<Utc>) -> sqlx::Result<Option<SessionRow>>;
    /// The sessions waiting for their usage limit, for after a restart.
    async fn limited_sessions(&self) -> sqlx::Result<Vec<(Uuid, DateTime<Utc>)>>;
    /// The timezone of a session's owner, for the times its agent is told.
    async fn owner_zone(&self, owner: &str) -> sqlx::Result<chrono_tz::Tz>;
}

pub struct PgStore(pub PgPool);

#[async_trait]
impl SessionStore for PgStore {
    async fn get_session(&self, id: Uuid) -> sqlx::Result<Option<SessionRow>> {
        db::get_session(&self.0, id).await
    }

    async fn owner_zone(&self, owner: &str) -> sqlx::Result<chrono_tz::Tz> {
        db::user_zone(&self.0, owner).await
    }

    async fn set_phase(&self, id: Uuid, phase: Phase) -> sqlx::Result<Option<SessionRow>> {
        db::set_phase(&self.0, id, phase).await
    }

    async fn set_auto_title(&self, id: Uuid, title: &str) -> sqlx::Result<bool> {
        db::set_auto_title(&self.0, id, title).await
    }

    async fn set_session_model(&self, id: Uuid, model: &str) -> sqlx::Result<()> {
        db::set_session_model(&self.0, id, model).await
    }

    async fn append_event(&self, id: Uuid, payload: &Value) -> sqlx::Result<i64> {
        db::append_event(&self.0, id, payload).await
    }

    async fn last_event(&self, id: Uuid) -> sqlx::Result<i64> {
        db::last_event(&self.0, id).await
    }

    async fn has_message(&self, id: Uuid, message_id: Uuid) -> sqlx::Result<bool> {
        db::has_message(&self.0, id, message_id).await
    }

    async fn remove_message(&self, id: Uuid, message_id: Uuid) -> sqlx::Result<()> {
        db::remove_message(&self.0, id, message_id).await
    }

    async fn message_removed(&self, id: Uuid, message_id: Uuid) -> sqlx::Result<bool> {
        db::message_removed(&self.0, id, message_id).await
    }

    async fn events_before(
        &self,
        id: Uuid,
        before: i64,
        limit: i64,
    ) -> sqlx::Result<Vec<(i64, Value)>> {
        db::events_before(&self.0, id, before, limit).await
    }

    async fn pending_prompt(&self, id: Uuid) -> sqlx::Result<Option<String>> {
        db::pending_prompt(&self.0, id).await
    }

    async fn save_queue(&self, id: Uuid, queue: &[Value]) -> sqlx::Result<()> {
        db::save_queue(&self.0, id, queue).await
    }

    async fn queues(&self) -> sqlx::Result<Vec<(Uuid, Vec<Value>)>> {
        db::queues(&self.0).await
    }

    async fn save_question(&self, id: Uuid, question: Uuid, questions: &Value) -> sqlx::Result<()> {
        db::save_question(&self.0, id, question, questions).await
    }

    async fn forget_question(&self, question: Uuid) -> sqlx::Result<()> {
        db::forget_question(&self.0, question).await
    }

    async fn questions(&self) -> sqlx::Result<Vec<(Uuid, Uuid, Value)>> {
        db::questions(&self.0).await
    }

    async fn clear_pending_prompt(&self, id: Uuid) -> sqlx::Result<()> {
        db::clear_pending_prompt(&self.0, id).await
    }

    async fn set_limited_until(
        &self,
        id: Uuid,
        until: Option<DateTime<Utc>>,
    ) -> sqlx::Result<Option<SessionRow>> {
        db::set_limited_until(&self.0, id, until).await
    }

    async fn end_limit(&self, id: Uuid, at: DateTime<Utc>) -> sqlx::Result<Option<SessionRow>> {
        db::end_limit(&self.0, id, at).await
    }

    async fn limited_sessions(&self) -> sqlx::Result<Vec<(Uuid, DateTime<Utc>)>> {
        db::limited_sessions(&self.0).await
    }
}

#[cfg(test)]
pub mod memory {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use super::*;

    /// Sessions and their event logs in memory.
    #[derive(Default)]
    pub struct MemoryStore {
        /// The waits for a usage limit ended, and when, for tests.
        pub limits_ended: Mutex<Vec<(Uuid, DateTime<Utc>)>>,
        sessions: Mutex<HashMap<Uuid, SessionRow>>,
        events: Mutex<Vec<(Uuid, i64, Value)>>,
        /// Sessions' kept queues.
        pub queues: Mutex<HashMap<Uuid, Vec<Value>>>,
        /// Messages taken back out of sessions' queues.
        removed: Mutex<HashSet<(Uuid, Uuid)>>,
        /// Questions from `ask_user`, until they're answered.
        pub questions: Mutex<Vec<(Uuid, Uuid, Value)>>,
        /// Sessions' first messages, until a turn starts with them.
        pub pending_prompts: Mutex<HashMap<Uuid, String>>,
        /// Sessions whose first message a turn has started with.
        pub cleared_prompts: Mutex<Vec<Uuid>>,
        /// Makes clearing a first message fail, as a lost database would.
        pub fail_clearing: std::sync::atomic::AtomicBool,
        /// Owners' timezones; UTC for the rest.
        pub zones: Mutex<HashMap<String, chrono_tz::Tz>>,
        /// Makes reading unanswered questions fail, as a lost database would.
        pub fail_reading_questions: std::sync::atomic::AtomicBool,
        /// Makes saving this phase fail, as a lost database would.
        pub fail_phase: Mutex<Option<Phase>>,
        /// Makes keeping a session's queue fail, as a lost database would.
        pub fail_saving_queues: std::sync::atomic::AtomicBool,
        /// Makes reading whether a message was taken back fail, as a lost database would.
        pub fail_removal_lookups: std::sync::atomic::AtomicBool,
        /// Holds taking a message back, while its state lock is held, until `removals_released`; says when one
        /// reaches it with `removal_started`.
        pub gate_removals: std::sync::atomic::AtomicBool,
        pub removal_started: tokio::sync::Notify,
        pub removals_released: tokio::sync::Notify,
    }

    impl MemoryStore {
        pub fn add_session(&self, row: SessionRow) {
            self.sessions.lock().unwrap().insert(row.id, row);
        }

        /// The session's recorded `session/update` payloads, in order.
        pub fn updates(&self, id: Uuid) -> Vec<Value> {
            let events = self.events.lock().unwrap();
            events
                .iter()
                .filter(|(s, _, _)| *s == id)
                .map(|(_, _, p)| p.clone())
                .collect()
        }
    }

    #[async_trait]
    impl SessionStore for MemoryStore {
        async fn get_session(&self, id: Uuid) -> sqlx::Result<Option<SessionRow>> {
            Ok(self.sessions.lock().unwrap().get(&id).cloned())
        }

        async fn owner_zone(&self, owner: &str) -> sqlx::Result<chrono_tz::Tz> {
            Ok(self
                .zones
                .lock()
                .unwrap()
                .get(owner)
                .copied()
                .unwrap_or(chrono_tz::UTC))
        }

        async fn set_phase(&self, id: Uuid, phase: Phase) -> sqlx::Result<Option<SessionRow>> {
            if *self.fail_phase.lock().unwrap() == Some(phase) {
                return Err(sqlx::Error::PoolTimedOut);
            }
            let mut sessions = self.sessions.lock().unwrap();
            Ok(sessions.get_mut(&id).map(|row| {
                row.phase = phase.as_str().to_owned();
                row.clone()
            }))
        }

        async fn set_auto_title(&self, id: Uuid, title: &str) -> sqlx::Result<bool> {
            let mut sessions = self.sessions.lock().unwrap();
            Ok(sessions.get_mut(&id).is_some_and(|row| {
                let changed = row.title != title;
                row.title = title.to_owned();
                changed
            }))
        }

        async fn set_session_model(&self, id: Uuid, model: &str) -> sqlx::Result<()> {
            if let Some(row) = self.sessions.lock().unwrap().get_mut(&id) {
                row.model = Some(model.to_owned());
            }
            Ok(())
        }

        async fn append_event(&self, id: Uuid, payload: &Value) -> sqlx::Result<i64> {
            let mut events = self.events.lock().unwrap();
            let seq = events.len() as i64 + 1;
            events.push((id, seq, payload.clone()));
            Ok(seq)
        }

        async fn has_message(&self, id: Uuid, message_id: Uuid) -> sqlx::Result<bool> {
            let events = self.events.lock().unwrap();
            Ok(events.iter().any(|(s, _, p)| {
                *s == id
                    && p["sessionUpdate"] == "user_message_chunk"
                    && p["messageId"] == message_id.to_string().as_str()
            }))
        }

        async fn remove_message(&self, id: Uuid, message_id: Uuid) -> sqlx::Result<()> {
            if self.gate_removals.load(std::sync::atomic::Ordering::SeqCst) {
                self.removal_started.notify_one();
                self.removals_released.notified().await;
            }
            self.removed.lock().unwrap().insert((id, message_id));
            Ok(())
        }

        async fn message_removed(&self, id: Uuid, message_id: Uuid) -> sqlx::Result<bool> {
            if self
                .fail_removal_lookups
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(sqlx::Error::PoolTimedOut);
            }
            Ok(self.removed.lock().unwrap().contains(&(id, message_id)))
        }

        async fn last_event(&self, id: Uuid) -> sqlx::Result<i64> {
            let events = self.events.lock().unwrap();
            Ok(events
                .iter()
                .filter(|(s, _, _)| *s == id)
                .map(|(_, seq, _)| *seq)
                .max()
                .unwrap_or(0))
        }

        async fn events_before(
            &self,
            id: Uuid,
            before: i64,
            limit: i64,
        ) -> sqlx::Result<Vec<(i64, Value)>> {
            let events = self.events.lock().unwrap();
            let mut latest: Vec<(i64, Value)> = events
                .iter()
                .filter(|(s, seq, _)| *s == id && *seq < before)
                .rev()
                .take(limit as usize)
                .map(|(_, seq, p)| (*seq, p.clone()))
                .collect();
            latest.reverse();
            Ok(latest)
        }

        async fn save_queue(&self, id: Uuid, queue: &[Value]) -> sqlx::Result<()> {
            if self
                .fail_saving_queues
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(sqlx::Error::PoolTimedOut);
            }
            let mut queues = self.queues.lock().unwrap();
            if queue.is_empty() {
                queues.remove(&id);
            } else {
                queues.insert(id, queue.to_vec());
            }
            Ok(())
        }

        async fn queues(&self) -> sqlx::Result<Vec<(Uuid, Vec<Value>)>> {
            Ok(self.queues.lock().unwrap().clone().into_iter().collect())
        }

        async fn save_question(
            &self,
            id: Uuid,
            question: Uuid,
            questions: &Value,
        ) -> sqlx::Result<()> {
            self.questions
                .lock()
                .unwrap()
                .push((id, question, questions.clone()));
            Ok(())
        }

        async fn forget_question(&self, question: Uuid) -> sqlx::Result<()> {
            self.questions
                .lock()
                .unwrap()
                .retain(|(_, q, _)| *q != question);
            Ok(())
        }

        async fn questions(&self) -> sqlx::Result<Vec<(Uuid, Uuid, Value)>> {
            if self
                .fail_reading_questions
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(sqlx::Error::PoolTimedOut);
            }
            Ok(self.questions.lock().unwrap().clone())
        }

        async fn pending_prompt(&self, id: Uuid) -> sqlx::Result<Option<String>> {
            Ok(self.pending_prompts.lock().unwrap().get(&id).cloned())
        }

        async fn clear_pending_prompt(&self, id: Uuid) -> sqlx::Result<()> {
            if self.fail_clearing.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(sqlx::Error::PoolTimedOut);
            }
            self.pending_prompts.lock().unwrap().remove(&id);
            self.cleared_prompts.lock().unwrap().push(id);
            Ok(())
        }

        async fn set_limited_until(
            &self,
            id: Uuid,
            until: Option<DateTime<Utc>>,
        ) -> sqlx::Result<Option<SessionRow>> {
            let mut sessions = self.sessions.lock().unwrap();
            Ok(sessions.get_mut(&id).map(|row| {
                row.limited_until = until;
                row.clone()
            }))
        }

        async fn end_limit(&self, id: Uuid, at: DateTime<Utc>) -> sqlx::Result<Option<SessionRow>> {
            self.limits_ended.lock().unwrap().push((id, at));
            self.set_limited_until(id, None).await
        }

        async fn limited_sessions(&self) -> sqlx::Result<Vec<(Uuid, DateTime<Utc>)>> {
            let sessions = self.sessions.lock().unwrap();
            Ok(sessions
                .values()
                .filter_map(|row| row.limited_until.map(|until| (row.id, until)))
                .collect())
        }
    }
}
