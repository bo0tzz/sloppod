//! Tests of the queries against a real Postgres; `mise run check` provides one through `DATABASE_URL`.

use chrono::{Duration, Utc};
use serde_json::json;
use sloppod_protocol::api::{ProposalStatus, ReviewPolicy, SessionKind};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{self, NewSessionRow};

async fn session(db: &PgPool, owner: &str) -> Uuid {
    session_of_kind(db, owner, SessionKind::Work).await
}

async fn session_of_kind(db: &PgPool, owner: &str, kind: SessionKind) -> Uuid {
    let id = Uuid::now_v7();
    db::insert_session(
        db,
        NewSessionRow {
            id,
            owner,
            title: "test",
            custom_title: false,
            pending_prompt: "",
            agent: "claude-code",
            kind,
            review: ReviewPolicy::Off,
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
            db::insert_proposal(
                &db,
                s,
                "github.issue.create",
                ProposalStatus::Pending,
                &json!({}),
            )
            .await
            .unwrap()
            .id,
        );
    }
    for &id in &ids[..3] {
        db::decide_proposal(&db, id, ProposalStatus::Approved, &json!({}), None, "dev")
            .await
            .unwrap()
            .unwrap();
    }
    db::finish_proposal(
        &db,
        ids[0],
        ProposalStatus::Executed,
        Some(&json!({})),
        None,
    )
    .await
    .unwrap();
    assert!(
        db::decide_proposal(
            &db,
            ids[1],
            ProposalStatus::Rejected,
            &json!({}),
            None,
            "dev"
        )
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

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn review_rounds_track_a_proposal(db: PgPool) {
    let author = session(&db, "dev").await;
    let reviewer = session_of_kind(&db, "dev", SessionKind::Review).await;
    let listed: Vec<Uuid> = db::list_sessions(&db, "dev")
        .await
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(listed, [author]);

    let p = db::insert_proposal(
        &db,
        author,
        "github.pr.create",
        ProposalStatus::InReview,
        &json!({"n": 1}),
    )
    .await
    .unwrap()
    .id;
    db::set_review_session(&db, p, reviewer).await.unwrap();
    assert_eq!(
        db::reviewed_proposal(&db, reviewer)
            .await
            .unwrap()
            .map(|p| p.id),
        Some(p)
    );
    assert_eq!(
        db::start_review_round(&db, p, "aaa", None).await.unwrap(),
        1
    );
    assert!(
        db::finish_review_round(
            &db,
            p,
            "changes",
            "Fix it.",
            &json!([{"id": "1.1"}]),
            &json!([])
        )
        .await
        .unwrap()
        .is_some()
    );
    assert!(
        db::finish_review_round(&db, p, "pass", "Again.", &json!([]), &json!([]))
            .await
            .unwrap()
            .is_none()
    );
    let moved = db::move_proposal(
        &db,
        p,
        ProposalStatus::InReview,
        ProposalStatus::ChangesRequested,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(moved.payload, json!({"n": 1}));
    assert!(
        db::move_proposal(
            &db,
            p,
            ProposalStatus::InReview,
            ProposalStatus::Pending,
            None
        )
        .await
        .unwrap()
        .is_none()
    );

    let moved = db::move_proposal(
        &db,
        p,
        ProposalStatus::ChangesRequested,
        ProposalStatus::InReview,
        Some(&json!({"n": 2})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(moved.payload, json!({"n": 2}));
    assert_eq!(
        db::start_review_round(&db, p, "bbb", Some("No."))
            .await
            .unwrap(),
        2
    );
    let rounds = db::reviews(&db, p).await.unwrap();
    assert_eq!(rounds.len(), 2);
    assert_eq!(rounds[0].verdict.as_deref(), Some("changes"));
    assert_eq!(rounds[1].replies.as_deref(), Some("No."));
    assert_eq!(rounds[1].verdict, None);

    // Proposals in review can be withdrawn like pending ones.
    assert!(
        db::withdraw_proposal(&db, p, author, None)
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn review_policy_defaults_to_off(db: PgPool) {
    assert_eq!(
        db::user_review(&db, "dev").await.unwrap(),
        ReviewPolicy::Off
    );
    db::set_user_model(&db, "dev", Some("haiku")).await.unwrap();
    db::set_user_review(&db, "dev", ReviewPolicy::Required)
        .await
        .unwrap();
    assert_eq!(
        db::user_review(&db, "dev").await.unwrap(),
        ReviewPolicy::Required
    );
    assert_eq!(
        db::user_model(&db, "dev").await.unwrap().as_deref(),
        Some("haiku")
    );

    let s = session(&db, "dev").await;
    db::set_session_review(&db, s, ReviewPolicy::Required)
        .await
        .unwrap();
    let row = db::get_session(&db, s).await.unwrap().unwrap();
    assert_eq!(row.review, ReviewPolicy::Required);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn findings_from_before_severities_still_read(db: PgPool) {
    let s = session(&db, "dev").await;
    let p = db::insert_proposal(
        &db,
        s,
        "github.pr.create",
        ProposalStatus::ChangesRequested,
        &json!({}),
    )
    .await
    .unwrap()
    .id;
    db::start_review_round(&db, p, "aaa", None).await.unwrap();
    sqlx::query("update reviews set verdict = 'changes', findings = $2 where proposal_id = $1")
        .bind(p)
        .bind(json!([
            {"path": "src/a.rs", "line": 3, "severity": "high", "comment": "Off by one.\nThe last item is skipped."},
            {"severity": "low", "comment": "No test."},
        ]))
        .execute(&db)
        .await
        .unwrap();

    sqlx::raw_sql(
        include_str!("../migrations/0011_review_findings.sql")
            .split_once(";\n")
            .unwrap()
            .1,
    )
    .execute(&db)
    .await
    .unwrap();

    let findings = db::reviews(&db, p).await.unwrap().remove(0).findings;
    assert_eq!(
        findings,
        json!([
            {"id": "1.1", "severity": "p1", "category": "correctness", "title": "Off by one.",
             "explanation": "Off by one.\nThe last item is skipped.", "path": "src/a.rs", "line": 3},
            {"id": "1.2", "severity": "p2", "category": "correctness", "title": "No test.", "explanation": "No test."},
        ])
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn an_agent_failure_is_news_once_until_the_agent_is_ready(db: PgPool) {
    let s = session(&db, "dev").await;
    db::set_agent_status(&db, s, "cloning").await.unwrap();
    let row = db::get_session(&db, s).await.unwrap().unwrap().to_api();
    assert_eq!(row.agent_status.as_deref(), Some("cloning"));
    assert!(db::set_agent_error(&db, s, "clone failed").await.unwrap());
    // The next attempt fails the same way: not news, but its status still clears.
    db::set_agent_status(&db, s, "cloning").await.unwrap();
    assert!(!db::set_agent_error(&db, s, "clone failed").await.unwrap());
    let row = db::get_session(&db, s).await.unwrap().unwrap().to_api();
    assert_eq!(row.agent_status, None);
    assert_eq!(row.agent_error.as_deref(), Some("clone failed"));

    db::clear_agent_status(&db, s).await.unwrap();
    let row = db::get_session(&db, s).await.unwrap().unwrap().to_api();
    assert_eq!((row.agent_status, row.agent_error), (None, None));
    // After a start that worked, the same failure is news again.
    assert!(db::set_agent_error(&db, s, "clone failed").await.unwrap());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn parked_sessions_are_left_alone_on_startup(db: PgPool) {
    use sloppod_protocol::api::Phase;

    let running = session(&db, "dev").await;
    let parked = session(&db, "dev").await;
    let deleted = session(&db, "dev").await;
    db::set_phase(&db, parked, Phase::Parked).await.unwrap();
    db::set_phase(&db, deleted, Phase::Stopped).await.unwrap();
    let active: Vec<Uuid> = db::active_sessions(&db)
        .await
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(active, [running]);
    assert_eq!(
        db::get_session(&db, parked).await.unwrap().unwrap().phase(),
        Phase::Parked
    );
    // Their egress policies are swept instead.
    let swept: Vec<Uuid> = db::parked_sessions(&db)
        .await
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(swept, [parked]);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn finds_proposals_still_with_their_reviewer(db: PgPool) {
    let s = session(&db, "dev").await;
    let mut ids = Vec::new();
    for status in [
        ProposalStatus::InReview,
        ProposalStatus::ChangesRequested,
        ProposalStatus::Pending,
    ] {
        ids.push(
            db::insert_proposal(&db, s, "github.pr.create", status, &json!({}))
                .await
                .unwrap()
                .id,
        );
    }
    let found: Vec<Uuid> = db::proposals_in_review(&db, s)
        .await
        .unwrap()
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(found, &ids[..2]);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn deleted_sessions_stay_restorable_until_purged(db: PgPool) {
    let kept = session(&db, "dev").await;
    let deleted = session(&db, "dev").await;
    db::mark_deleted(&db, deleted).await.unwrap();
    let listed = |rows: Vec<db::SessionRow>| rows.iter().map(|s| s.id).collect::<Vec<_>>();
    assert_eq!(listed(db::list_sessions(&db, "dev").await.unwrap()), [kept]);
    assert_eq!(
        listed(db::deleted_sessions(&db, "dev").await.unwrap()),
        [deleted]
    );
    assert!(
        db::claim_purges(&db, chrono::Duration::days(7))
            .await
            .unwrap()
            .is_empty()
    );

    // Restored, it's back among the sessions and no purge claims it.
    assert!(db::restore(&db, deleted).await.unwrap());
    assert!(!db::restore(&db, deleted).await.unwrap());
    assert!(
        db::claim_purges(&db, chrono::Duration::zero())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        listed(db::list_sessions(&db, "dev").await.unwrap()).len(),
        2
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_purge_claim_wins_over_restoring_and_is_retried_until_done(db: PgPool) {
    let s = session(&db, "dev").await;
    db::mark_deleted(&db, s).await.unwrap();
    let claimed = |rows: Vec<db::SessionRow>| rows.iter().map(|s| s.id).collect::<Vec<_>>();
    assert_eq!(
        claimed(
            db::claim_purges(&db, chrono::Duration::zero())
                .await
                .unwrap()
        ),
        [s]
    );

    // Claimed, it can't be restored, and it's no longer offered for restoring.
    assert!(!db::restore(&db, s).await.unwrap());
    assert!(db::deleted_sessions(&db, "dev").await.unwrap().is_empty());

    // Cleanup failed, so the next sweep claims it again; once done, never again.
    assert_eq!(
        claimed(
            db::claim_purges(&db, chrono::Duration::zero())
                .await
                .unwrap()
        ),
        [s]
    );
    db::finish_purge(&db, s).await.unwrap();
    assert!(
        db::claim_purges(&db, chrono::Duration::zero())
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn finds_stacks_set_to_merge(db: PgPool) {
    let s = session(&db, "dev").await;
    let mut ids = Vec::new();
    for (kind, auto_merge) in [
        ("github.stack.create", true),
        ("github.stack.create", false),
        ("github.pr.create", true),
    ] {
        let id = db::insert_proposal(&db, s, kind, ProposalStatus::Pending, &json!({}))
            .await
            .unwrap()
            .id;
        db::decide_proposal(&db, id, ProposalStatus::Approved, &json!({}), None, "dev")
            .await
            .unwrap()
            .unwrap();
        db::finish_proposal(
            &db,
            id,
            ProposalStatus::Executed,
            Some(&json!({"auto_merge": auto_merge})),
            None,
        )
        .await
        .unwrap();
        ids.push(id);
    }
    let found: Vec<Uuid> = db::merging_stacks(&db)
        .await
        .unwrap()
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(found, [ids[0]]);
}
