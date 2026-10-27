//! Tests of the queries against a real Postgres; `mise run check` provides one through `DATABASE_URL`.

use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{self, NewSessionRow};

async fn session(db: &PgPool, owner: &str) -> Uuid {
    let id = Uuid::now_v7();
    db::insert_session(
        db,
        NewSessionRow {
            id,
            owner,
            title: "test",
            custom_title: false,
            pending_prompt: "",
            model: None,
            repo: None,
            grants: &[],
        },
    )
    .await
    .unwrap();
    id
}

async fn approval(
    db: &PgPool,
    owner: &str,
    kind: &str,
    target: Option<&str>,
    session_id: Option<Uuid>,
) -> Uuid {
    db::insert_standing_approval(db, owner, kind, target, session_id, &json!({}), None)
        .await
        .unwrap()
        .id
}

async fn matching(db: &PgPool, owner: &str, target: &str, session_id: Uuid) -> Option<Uuid> {
    db::matching_standing_approval(db, owner, "github.pr.create", Some(target), session_id)
        .await
        .unwrap()
        .map(|a| a.id)
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn standing_approvals_match_on_kind_target_and_session(db: PgPool) {
    let mine = session(&db, "dev").await;
    let other = session(&db, "dev").await;
    let scoped = approval(
        &db,
        "dev",
        "github.pr.create",
        Some("bo0tzz/Sloppod"),
        Some(mine),
    )
    .await;
    approval(&db, "dev", "github.issue.create", None, None).await;
    approval(&db, "someone", "github.pr.create", None, None).await;

    assert_eq!(
        matching(&db, "dev", "BO0TZZ/sloppod", mine).await,
        Some(scoped)
    );
    assert_eq!(matching(&db, "dev", "bo0tzz/other", mine).await, None);
    assert_eq!(matching(&db, "dev", "bo0tzz/sloppod", other).await, None);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn the_oldest_live_standing_approval_matches(db: PgPool) {
    let s = session(&db, "dev").await;
    let revoked = approval(&db, "dev", "github.pr.create", None, None).await;
    db::insert_standing_approval(
        &db,
        "dev",
        "github.pr.create",
        None,
        None,
        &json!({}),
        Some(Utc::now() - Duration::minutes(1)),
    )
    .await
    .unwrap();
    let live = approval(&db, "dev", "github.pr.create", None, None).await;
    approval(&db, "dev", "github.pr.create", Some("bo0tzz/sloppod"), None).await;

    assert_eq!(
        matching(&db, "dev", "bo0tzz/sloppod", s).await,
        Some(revoked)
    );
    assert!(
        db::revoke_standing_approval(&db, "dev", revoked)
            .await
            .unwrap()
    );
    assert!(
        !db::revoke_standing_approval(&db, "dev", revoked)
            .await
            .unwrap()
    );
    assert_eq!(matching(&db, "dev", "bo0tzz/sloppod", s).await, Some(live));
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn only_approved_proposals_are_resumed(db: PgPool) {
    let s = session(&db, "dev").await;
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(
            db::insert_proposal(&db, s, "github.issue.create", &json!({}))
                .await
                .unwrap()
                .id,
        );
    }
    for &id in &ids[..3] {
        db::decide_proposal(&db, id, "approved", &json!({}), None, "dev")
            .await
            .unwrap()
            .unwrap();
    }
    db::finish_proposal(&db, ids[0], "done", Some(&json!({})), None)
        .await
        .unwrap();
    assert!(
        db::decide_proposal(&db, ids[1], "rejected", &json!({}), None, "dev")
            .await
            .unwrap()
            .is_none()
    );

    let resumed: Vec<Uuid> = db::approved_proposals(&db)
        .await
        .unwrap()
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(resumed, &ids[1..3]);
}
