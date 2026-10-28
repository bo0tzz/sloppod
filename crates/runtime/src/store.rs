//! What the session hub keeps in the database, behind a trait so the hub can run against memory in tests.

use async_trait::async_trait;
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
    async fn events(&self, id: Uuid) -> sqlx::Result<Vec<(i64, Value)>>;
    /// Forgets the session's first message, once a turn has started with it.
    async fn clear_pending_prompt(&self, id: Uuid) -> sqlx::Result<()>;
}

pub struct PgStore(pub PgPool);

#[async_trait]
impl SessionStore for PgStore {
    async fn get_session(&self, id: Uuid) -> sqlx::Result<Option<SessionRow>> {
        db::get_session(&self.0, id).await
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

    async fn events(&self, id: Uuid) -> sqlx::Result<Vec<(i64, Value)>> {
        db::events(&self.0, id).await
    }

    async fn clear_pending_prompt(&self, id: Uuid) -> sqlx::Result<()> {
        db::clear_pending_prompt(&self.0, id).await
    }
}

#[cfg(test)]
pub mod memory {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// Sessions and their event logs in memory.
    #[derive(Default)]
    pub struct MemoryStore {
        sessions: Mutex<HashMap<Uuid, SessionRow>>,
        events: Mutex<Vec<(Uuid, i64, Value)>>,
        /// Sessions whose first message a turn has started with.
        pub cleared_prompts: Mutex<Vec<Uuid>>,
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

        async fn set_phase(&self, id: Uuid, phase: Phase) -> sqlx::Result<Option<SessionRow>> {
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

        async fn events(&self, id: Uuid) -> sqlx::Result<Vec<(i64, Value)>> {
            let events = self.events.lock().unwrap();
            Ok(events
                .iter()
                .filter(|(s, _, _)| *s == id)
                .map(|(_, seq, p)| (*seq, p.clone()))
                .collect())
        }

        async fn clear_pending_prompt(&self, id: Uuid) -> sqlx::Result<()> {
            self.cleared_prompts.lock().unwrap().push(id);
            Ok(())
        }
    }
}
