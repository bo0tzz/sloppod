//! Tests of the queries against a real Postgres; `mise run check` provides one through `DATABASE_URL`.

use chrono::{Duration, Utc};
use serde_json::json;
use sloppod_protocol::api::{Phase, ProposalStatus, ReviewPolicy, SessionKind};
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
async fn a_question_is_kept_until_forgotten(db: PgPool) {
    let s = session(&db, "dev").await;
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    let questions = json!([{"question": "Which?"}]);
    db::save_question(&db, s, first, &questions).await.unwrap();
    db::save_question(&db, s, second, &questions).await.unwrap();
    db::forget_question(&db, first).await.unwrap();
    assert_eq!(db::questions(&db).await.unwrap(), [(s, second, questions)]);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_queue_is_kept_until_emptied(db: PgPool) {
    let s = session(&db, "dev").await;
    let queue = [json!({"id": null, "blocks": [{"type": "text", "text": "hi"}]})];
    db::save_queue(&db, s, &queue).await.unwrap();
    assert_eq!(db::queues(&db).await.unwrap(), [(s, queue.to_vec())]);
    db::save_queue(&db, s, &[]).await.unwrap();
    assert!(db::queues(&db).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn remembers_messages_taken_back_per_session(db: PgPool) {
    let (s, other) = (session(&db, "dev").await, session(&db, "dev").await);
    let message = Uuid::new_v4();
    assert!(!db::message_removed(&db, s, message).await.unwrap());
    db::remove_message(&db, s, message).await.unwrap();
    db::remove_message(&db, s, message).await.unwrap();
    assert!(db::message_removed(&db, s, message).await.unwrap());
    assert!(!db::message_removed(&db, other, message).await.unwrap());
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

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_transcript_reads_a_chunk_at_a_time(db: PgPool) {
    let s = session(&db, "dev").await;
    db::upsert_transcript(&db, s, "b.jsonl", b"0123456789")
        .await
        .unwrap();
    db::upsert_transcript(&db, s, "a.jsonl", b"x")
        .await
        .unwrap();
    assert_eq!(
        db::transcript_lens(&db, s).await.unwrap(),
        [("a.jsonl".to_owned(), 1), ("b.jsonl".to_owned(), 10)]
    );
    assert_eq!(
        db::transcript_len(&db, s, "b.jsonl").await.unwrap(),
        Some(10)
    );
    assert_eq!(db::transcript_len(&db, s, "c.jsonl").await.unwrap(), None);
    let mut read = Vec::new();
    let mut offset = 0;
    loop {
        let chunk = db::transcript_chunk(&db, s, "b.jsonl", offset, 4)
            .await
            .unwrap();
        if chunk.is_empty() {
            break;
        }
        offset += chunk.len() as i64;
        read.extend(chunk);
    }
    assert_eq!(read, b"0123456789");
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_replacement_in_pieces_keeps_the_old_copy_until_it_is_whole(db: PgPool) {
    use std::collections::HashMap;

    use crate::link::store_transcript_piece;

    let s = session(&db, "dev").await;
    db::upsert_transcript(&db, s, "t.jsonl", b"old copy")
        .await
        .unwrap();
    let stored = |db: PgPool| async move { db::transcripts(&db, s).await.unwrap() };

    // Cut off after the first piece: the old copy is what's restored.
    let mut staging = HashMap::new();
    assert!(
        store_transcript_piece(&db, s, &mut staging, "t.jsonl", 0, b"new ", Some(8))
            .await
            .unwrap()
    );
    assert_eq!(
        stored(db.clone()).await,
        [("t.jsonl".to_owned(), b"old copy".to_vec())]
    );
    assert!(db::transcript_lens(&db, s).await.unwrap().len() == 1);

    // A link that starts over sends it whole again, and the last piece swaps it in.
    let mut staging = HashMap::new();
    for (offset, piece, replaces) in [(0, &b"new "[..], Some(8)), (4, &b"copy"[..], None)] {
        assert!(
            store_transcript_piece(&db, s, &mut staging, "t.jsonl", offset, piece, replaces)
                .await
                .unwrap()
        );
    }
    assert_eq!(
        stored(db.clone()).await,
        [("t.jsonl".to_owned(), b"new copy".to_vec())]
    );
    assert!(staging.is_empty());

    // Appends after it go to the file as usual.
    assert!(
        store_transcript_piece(&db, s, &mut staging, "t.jsonl", 8, b"!", None)
            .await
            .unwrap()
    );
    assert_eq!(
        db::transcript_lens(&db, s).await.unwrap(),
        [("t.jsonl".to_owned(), 9)]
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_session_log_reads_back_from_its_latest(db: PgPool) {
    let s = session(&db, "dev").await;
    let other = session(&db, "dev").await;
    let mut seqs = Vec::new();
    for n in 0..5 {
        seqs.push(db::append_event(&db, s, &json!({"n": n})).await.unwrap());
        db::append_event(&db, other, &json!({"other": n}))
            .await
            .unwrap();
    }
    let latest = db::events_before(&db, s, i64::MAX, 2).await.unwrap();
    assert_eq!(
        latest.iter().map(|e| e.1["n"].clone()).collect::<Vec<_>>(),
        [json!(3), json!(4)]
    );
    let older = db::events_before(&db, s, latest[0].0, 10).await.unwrap();
    assert_eq!(older.iter().map(|e| e.0).collect::<Vec<_>>(), seqs[..3]);
    assert!(
        db::events_before(&db, s, seqs[0], 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_session_log_has_a_last_event(db: PgPool) {
    let s = session(&db, "dev").await;
    assert_eq!(db::last_event(&db, s).await.unwrap(), 0);
    let seq = db::append_event(&db, s, &json!({})).await.unwrap();
    db::append_event(&db, session(&db, "dev").await, &json!({}))
        .await
        .unwrap();
    assert_eq!(db::last_event(&db, s).await.unwrap(), seq);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_pull_requests_fingerprint_is_found_by_repository_and_number(db: PgPool) {
    let s = session(&db, "dev").await;
    for (repo, number, fingerprint) in [("O/R", 7, Some("abc")), ("o/r", 8, None)] {
        let id = db::insert_proposal(
            &db,
            s,
            "github.pr.create",
            ProposalStatus::Pending,
            &json!({"repo": repo}),
        )
        .await
        .unwrap()
        .id;
        db::decide_proposal(
            &db,
            id,
            ProposalStatus::Approved,
            &json!({"repo": repo}),
            None,
            "dev",
        )
        .await
        .unwrap()
        .unwrap();
        let mut result = json!({"number": number, "auto_merge": true});
        if let Some(fingerprint) = fingerprint {
            result["fingerprint"] = json!(fingerprint);
        }
        db::finish_proposal(&db, id, ProposalStatus::Executed, Some(&result), None)
            .await
            .unwrap();
    }
    assert_eq!(
        db::pull_fingerprint(&db, "o/r", 7)
            .await
            .unwrap()
            .as_deref(),
        Some("abc")
    );
    assert_eq!(db::pull_fingerprint(&db, "o/r", 8).await.unwrap(), None);
    assert_eq!(db::pull_fingerprint(&db, "o/other", 7).await.unwrap(), None);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn only_a_live_session_acts(db: PgPool) {
    use sloppod_protocol::api::Phase;

    let s = session(&db, "dev").await;
    let row = || async { db::get_session(&db, s).await.unwrap().unwrap() };
    assert_eq!(row().await.not_acting(), None);
    for (phase, why) in [(Phase::Parked, "parked"), (Phase::Stopped, "stopped")] {
        db::set_phase(&db, s, phase).await.unwrap();
        assert_eq!(row().await.not_acting(), Some(why));
    }
    let deleted = session(&db, "dev").await;
    db::mark_deleted(&db, deleted).await.unwrap();
    let row = db::get_session(&db, deleted).await.unwrap().unwrap();
    assert_eq!(row.not_acting(), Some("deleted"));
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn the_pull_requests_sloppod_opened_are_watched_until_they_close(db: PgPool) {
    use sloppod_protocol::api::Phase;

    let s = session(&db, "dev").await;
    let stopped = session(&db, "dev").await;
    let executed =
        |session, kind: &'static str, payload: serde_json::Value, result: serde_json::Value| {
            let db = db.clone();
            async move {
                let id = db::insert_proposal(&db, session, kind, ProposalStatus::Pending, &payload)
                    .await
                    .unwrap()
                    .id;
                db::decide_proposal(&db, id, ProposalStatus::Approved, &payload, None, "dev")
                    .await
                    .unwrap()
                    .unwrap();
                db::finish_proposal(&db, id, ProposalStatus::Executed, Some(&result), None)
                    .await
                    .unwrap();
            }
        };
    executed(
        s,
        "github.pr.create",
        json!({"repo": "O/R"}),
        json!({"number": 1}),
    )
    .await;
    executed(
        s,
        "github.stack.create",
        json!({"repo": "o/r"}),
        json!({"pull_requests": [{"number": 2}, {"number": 3}]}),
    )
    .await;
    executed(
        stopped,
        "github.pr.create",
        json!({"repo": "o/r"}),
        json!({"number": 4}),
    )
    .await;
    db::set_phase(&db, stopped, Phase::Stopped).await.unwrap();

    let numbers = |pulls: Vec<db::WatchedPull>| pulls.iter().map(|p| p.number).collect::<Vec<_>>();
    let watched = db::watched_pulls(&db).await.unwrap();
    assert!(
        watched
            .iter()
            .all(|p| p.repo == "o/r" && p.observed.is_none())
    );
    assert_eq!(numbers(watched), [1, 2, 3]);

    db::save_pull_watch(
        &db,
        "o/r",
        1,
        s,
        &json!({"state": "OPEN", "checks": "SUCCESS"}),
    )
    .await
    .unwrap();
    db::save_pull_watch(&db, "o/r", 2, s, &json!({"state": "MERGED"}))
        .await
        .unwrap();
    let watched = db::watched_pulls(&db).await.unwrap();
    assert_eq!(watched[0].observed.as_ref().unwrap()["checks"], "SUCCESS");
    assert_eq!(
        numbers(watched),
        [1, 3],
        "a merged one isn't watched any more"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_tree_counts_its_running_sub_sessions_at_any_depth(db: PgPool) {
    let root = session(&db, "alice").await;
    let child = session(&db, "alice").await;
    let grandchild = session(&db, "alice").await;
    let parked = session(&db, "alice").await;
    let elsewhere = session(&db, "alice").await;
    db::set_parent(&db, child, root).await.unwrap();
    db::set_parent(&db, grandchild, child).await.unwrap();
    db::set_parent(&db, parked, root).await.unwrap();
    db::set_phase(&db, parked, Phase::Parked).await.unwrap();

    // The same count from anywhere in the tree; the root itself and other trees don't count.
    for member in [root, child, grandchild] {
        assert_eq!(db::running_in_tree(&db, member).await.unwrap(), 2);
    }
    assert_eq!(db::running_in_tree(&db, elsewhere).await.unwrap(), 0);
    let children: Vec<Uuid> = db::children(&db, root)
        .await
        .unwrap()
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(children, [child, parked]);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn an_owner_has_one_utility_session_that_isnt_listed(db: PgPool) {
    use sloppod_protocol::api::Phase;

    assert!(db::utility_session(&db, "dev").await.unwrap().is_none());
    let utility = session_of_kind(&db, "dev", SessionKind::Utility).await;
    session_of_kind(&db, "other", SessionKind::Work).await;
    assert_eq!(
        db::utility_session(&db, "dev").await.unwrap().map(|s| s.id),
        Some(utility)
    );
    assert!(db::utility_session(&db, "other").await.unwrap().is_none());
    // Not among the owner's sessions in the UI.
    assert!(db::list_sessions(&db, "dev").await.unwrap().is_empty());
    assert_eq!(db::utility_sessions(&db).await.unwrap().len(), 1);
    // A stopped one is no longer theirs to use.
    db::set_phase(&db, utility, Phase::Stopped).await.unwrap();
    assert!(db::utility_session(&db, "dev").await.unwrap().is_none());
    assert!(db::utility_sessions(&db).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_sessions_first_message_is_its_task(db: PgPool) {
    let id = session(&db, "alice").await;
    assert_eq!(db::first_message(&db, id).await.unwrap(), None);
    let message = |text: &str| json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": text}});
    db::append_event(
        &db,
        id,
        &json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}}),
    )
    .await
    .unwrap();
    db::append_event(&db, id, &message("Fix the build."))
        .await
        .unwrap();
    db::append_event(&db, id, &message("Also the docs."))
        .await
        .unwrap();
    assert_eq!(
        db::first_message(&db, id).await.unwrap().as_deref(),
        Some("Fix the build.")
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn live_descendants_are_found_below_deleted_ones(db: PgPool) {
    // A → B → C, with B deleted and C restored on its own.
    let a = session(&db, "alice").await;
    let b = session(&db, "alice").await;
    let c = session(&db, "alice").await;
    db::set_parent(&db, b, a).await.unwrap();
    db::set_parent(&db, c, b).await.unwrap();
    db::mark_deleted(&db, b).await.unwrap();
    let ids = |rows: Vec<db::SessionRow>| rows.iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(ids(db::live_descendants(&db, a).await.unwrap()), [c]);
    assert!(db::live_descendants(&db, c).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_session_below_a_deleted_one_is_in_a_deleted_line(db: PgPool) {
    // What stops a sub-session spawning a child while its tree is being deleted.
    let a = session(&db, "alice").await;
    let b = session(&db, "alice").await;
    let c = session(&db, "alice").await;
    db::set_parent(&db, b, a).await.unwrap();
    db::set_parent(&db, c, b).await.unwrap();
    assert!(!db::deleted_in_line(&db, c).await.unwrap());
    db::mark_deleted(&db, a).await.unwrap();
    for member in [a, b, c] {
        assert!(db::deleted_in_line(&db, member).await.unwrap());
    }
}

async fn credential(db: &PgPool, owner: &str, kind: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into credentials (id, owner, kind, name, secret, is_default) values ($1, $2, $3, $3, '\\x00', true)",
    )
    .bind(id)
    .bind(owner)
    .bind(kind)
    .execute(db)
    .await
    .unwrap();
    id
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_quota_keeps_the_windows_a_report_leaves_out(db: PgPool) {
    use crate::quota::{self, Observation, Window};
    use sloppod_protocol::api::QuotaStatus;
    let claude = credential(&db, "dev", "claude").await;
    credential(&db, "someone", "claude").await;
    let window = |utilization: f64, status| Window {
        utilization: Some(utilization),
        resets_at: Some(Utc::now() + Duration::hours(3)),
        status,
    };
    let first = Utc::now() - Duration::minutes(10);
    let both = Observation {
        observed_at: first,
        limited: false,
        resets_at: None,
        windows: [
            ("five_hour".to_owned(), window(0.5, QuotaStatus::Allowed)),
            ("seven_day".to_owned(), window(0.8, QuotaStatus::Warning)),
        ]
        .into(),
    };
    quota::store(&db, claude, &both).await.unwrap();
    let reset_at = Utc::now() + Duration::hours(1);
    let refused = Observation {
        observed_at: first + Duration::minutes(5),
        limited: true,
        resets_at: Some(reset_at),
        windows: [("five_hour".to_owned(), window(1.0, QuotaStatus::Rejected))].into(),
    };
    quota::store(&db, claude, &refused).await.unwrap();
    // A report from between the two that arrives late changes nothing they both had.
    let late = Observation {
        observed_at: first + Duration::minutes(1),
        limited: false,
        resets_at: None,
        windows: [("five_hour".to_owned(), window(0.6, QuotaStatus::Allowed))].into(),
    };
    quota::store(&db, claude, &late).await.unwrap();

    let quotas = quota::list(&db, "dev", Utc::now()).await.unwrap();
    assert_eq!(quotas.len(), 1);
    let quota = &quotas[0];
    assert_eq!(
        (quota.credential_id, quota.kind.as_str()),
        (claude, "claude")
    );
    assert!(quota.limited);
    let windows: Vec<_> = quota
        .windows
        .iter()
        .map(|w| (w.window.as_str(), w.utilization, w.status))
        .collect();
    assert_eq!(
        windows,
        [
            ("five_hour", Some(1.0), QuotaStatus::Rejected),
            ("seven_day", Some(0.8), QuotaStatus::Warning)
        ]
    );
    // Once the reset has passed, it's over.
    let later = quota::for_credential(&db, claude, reset_at + Duration::minutes(1))
        .await
        .unwrap()
        .unwrap();
    assert!(!later.limited);
    // Only the owner's own credentials, and only those with a report.
    assert!(
        quota::list(&db, "someone", Utc::now())
            .await
            .unwrap()
            .is_empty()
    );

    sqlx::query("delete from credentials where id = $1")
        .bind(claude)
        .execute(&db)
        .await
        .unwrap();
    assert!(
        quota::list(&db, "dev", Utc::now())
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn a_review_round_counts_its_wait_for_the_usage_limit_across_a_restart(db: PgPool) {
    use crate::reviews::{paused, round_timed_out};

    let author = session(&db, "dev").await;
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
    let round = db::start_review_round(&db, p, "aaa", None).await.unwrap();
    let started = db::reviews(&db, p).await.unwrap()[0].created_at;

    // The reviewer worked 10 minutes, then hit its usage limit; the runtime was down for the hour that followed,
    // and the limit reset meanwhile.
    let limited_at = started + Duration::minutes(10);
    db::pause_review(&db, p, round, limited_at).await.unwrap();
    db::pause_review(&db, p, round, limited_at + Duration::minutes(1))
        .await
        .unwrap();
    let back = limited_at + Duration::minutes(61);
    let row = &db::reviews(&db, p).await.unwrap()[0];
    assert_eq!(paused(row, back), Duration::minutes(61));

    // The first check after the restart ends the wait then, downtime included.
    db::unpause_review(&db, p, round, back).await.unwrap();
    let row = &db::reviews(&db, p).await.unwrap()[0];
    assert_eq!(row.paused_seconds, 61 * 60);
    assert_eq!(row.paused_since, None);
    // It keeps the 20 minutes of its 30 it hadn't used.
    let at = |minutes| back + Duration::minutes(minutes);
    assert!(!round_timed_out(
        row.created_at,
        paused(row, at(20)),
        false,
        at(20)
    ));
    assert!(round_timed_out(
        row.created_at,
        paused(row, at(21)),
        false,
        at(21)
    ));
}

#[sqlx::test]
async fn a_review_round_keeps_a_wait_the_runtime_went_down_in_before_its_watcher_saw_it(
    db: PgPool,
) {
    use crate::reviews::{paused, round_timed_out};

    let author = session(&db, "dev").await;
    let reviewer = session_of_kind(&db, "dev", SessionKind::Review).await;
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
    let round = db::start_review_round(&db, p, "aaa", None).await.unwrap();
    let started = db::reviews(&db, p).await.unwrap()[0].created_at;

    // The reviewer hits its limit after 10 minutes, and the runtime stops before the round's watcher looks again.
    let hit = started + Duration::minutes(10);
    db::pause_reviewer(&db, reviewer, hit).await.unwrap();
    // An hour later it's back, the limit reset meanwhile: the watcher's first check ends the wait.
    let back = hit + Duration::minutes(60);
    db::unpause_review(&db, p, round, back).await.unwrap();
    let row = &db::reviews(&db, p).await.unwrap()[0];
    assert_eq!(row.paused_seconds, 60 * 60);
    // The reviewer keeps the 20 minutes of its 30 it hadn't used.
    let at = |minutes| back + Duration::minutes(minutes);
    assert!(!round_timed_out(
        row.created_at,
        paused(row, at(20)),
        false,
        at(20)
    ));
    assert!(round_timed_out(
        row.created_at,
        paused(row, at(21)),
        false,
        at(21)
    ));

    // Ended in the same write as the reviewer session's wait, not at the watcher's next look: a restart later
    // counts no downtime.
    db::pause_reviewer(&db, reviewer, at(30)).await.unwrap();
    db::set_limited_until(&db, reviewer, Some(at(40)))
        .await
        .unwrap();
    let ended = db::end_limit(&db, reviewer, at(40)).await.unwrap().unwrap();
    assert_eq!(ended.limited_until, None);
    db::unpause_review(&db, p, round, at(100)).await.unwrap();
    let row = &db::reviews(&db, p).await.unwrap()[0];
    assert_eq!(row.paused_seconds, 70 * 60);
    assert_eq!(row.paused_since, None);

    // A round that has its verdict isn't paused.
    db::finish_review_round(&db, p, "approve", "Fine.", &json!([]), &json!([]))
        .await
        .unwrap();
    db::pause_reviewer(&db, reviewer, back).await.unwrap();
    assert_eq!(db::reviews(&db, p).await.unwrap()[0].paused_since, None);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_users_timezone_is_kept_and_utc_without_one(db: PgPool) {
    assert_eq!(db::user_timezone(&db, "dev").await.unwrap(), None);
    assert_eq!(db::user_zone(&db, "dev").await.unwrap(), chrono_tz::UTC);
    db::set_user_review(&db, "dev", ReviewPolicy::Required)
        .await
        .unwrap();
    db::set_user_timezone(&db, "dev", "Europe/Amsterdam")
        .await
        .unwrap();
    assert_eq!(
        db::user_timezone(&db, "dev").await.unwrap().as_deref(),
        Some("Europe/Amsterdam")
    );
    assert_eq!(
        db::user_zone(&db, "dev").await.unwrap(),
        chrono_tz::Europe::Amsterdam
    );
    // Setting it leaves the user's other settings alone.
    assert_eq!(
        db::user_review(&db, "dev").await.unwrap(),
        ReviewPolicy::Required
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_grant_can_run_out_be_narrowed_and_be_revoked(db: PgPool) {
    use sloppod_contracts::Grant;

    let s = session(&db, "dev").await;
    let capabilities = |row: &db::SessionRow| {
        row.grants()
            .iter()
            .map(|g| g.capability.clone())
            .collect::<Vec<_>>()
    };
    let soon = db::grant(
        &db,
        s,
        &Grant::new("packages", json!({})),
        Some(Utc::now() + Duration::hours(1)),
    )
    .await
    .unwrap();
    let repo = db::grant(
        &db,
        s,
        &Grant::new(
            "repo",
            json!({"repo": "a/b", "access": "rw", "branch": "sloppod/1"}),
        ),
        None,
    )
    .await
    .unwrap();
    let row = db::get_session(&db, s).await.unwrap().unwrap();
    assert_eq!(capabilities(&row), ["packages", "repo"]);
    let api = row.to_api();
    assert_eq!(api.grants[0].id, Some(soon));
    assert!(api.grants[0].expires_at.is_some());
    assert!(api.grants[1].expires_at.is_none());

    // Narrowed: the repository becomes read only.
    let narrower =
        crate::grants::narrowed(&db::session_grant(&db, s, repo).await.unwrap().unwrap()).unwrap();
    assert!(
        db::change_grant(&db, s, repo, Some(&narrower), None)
            .await
            .unwrap()
    );
    let row = db::get_session(&db, s).await.unwrap().unwrap();
    assert_eq!(row.grants()[1].param("access"), Some("ro"));

    // Run out: gone from the session at once, before the sweeper removes it.
    assert!(
        db::change_grant(
            &db,
            s,
            soon,
            None,
            Some(Some(Utc::now() - Duration::seconds(1)))
        )
        .await
        .unwrap()
    );
    let row = db::get_session(&db, s).await.unwrap().unwrap();
    assert_eq!(capabilities(&row), ["repo"]);
    assert!(db::session_grant(&db, s, soon).await.unwrap().is_none());
    let swept = db::sweep_expired_grants(&db).await.unwrap();
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0].0, s);
    assert_eq!(swept[0].1.capability, "packages");

    // Revoked: returned once, then gone; another owner's session can't be named.
    let revoked = db::revoke_grant(&db, s, repo).await.unwrap().unwrap();
    assert_eq!(revoked.capability, "repo");
    assert!(db::revoke_grant(&db, s, repo).await.unwrap().is_none());
    let other = session(&db, "someone").await;
    let theirs = db::grant(&db, other, &Grant::new("docs", json!({})), None)
        .await
        .unwrap();
    assert!(db::revoke_grant(&db, s, theirs).await.unwrap().is_none());
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_repository_grant_switches_to_the_owner_s_token_and_back(db: PgPool) {
    use sloppod_contracts::Grant;

    let id = session(&db, "dev").await;
    db::add_grant(
        &db,
        id,
        &Grant::new(
            "repo",
            json!({"repo": "Dev/App", "access": "rw", "branch": "sloppod/x"}),
        ),
    )
    .await
    .unwrap();
    assert!(
        db::set_repo_credential(&db, id, "dev/app", Some(("c1", "dev")))
            .await
            .unwrap()
    );
    let context = db::get_session(&db, id).await.unwrap().unwrap().context();
    assert_eq!(context.repo_credential("dev/app"), Some("c1"));
    assert_eq!(
        crate::grants::describe(&context.grants[0]),
        "Dev/App: read, and push the session branch `sloppod/x` and branches named after it, as dev (the user's own \
         GitHub token)."
    );
    assert_eq!(
        context.writable_repo_for("dev/app").map(|g| g.branch),
        Some("sloppod/x")
    );

    assert!(
        db::set_repo_credential(&db, id, "dev/app", None)
            .await
            .unwrap()
    );
    let context = db::get_session(&db, id).await.unwrap().unwrap().context();
    assert_eq!(context.repo_credential("dev/app"), None);
    assert!(
        !db::set_repo_credential(&db, id, "dev/other", None)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_sub_session_s_copy_of_a_grant_runs_out_with_it(db: PgPool) {
    use sloppod_contracts::Grant;

    // A parent's grant that runs out, as a sub-session gets it (its expiry travels with it).
    let parent = session(&db, "dev").await;
    let soon = Utc::now() + Duration::hours(1);
    db::grant(&db, parent, &Grant::new("packages", json!({})), Some(soon))
        .await
        .unwrap();
    let inherited = db::get_session(&db, parent)
        .await
        .unwrap()
        .unwrap()
        .grants();
    assert!(inherited[0].expires_at.is_some());
    let child = Uuid::now_v7();
    db::insert_session(
        &db,
        NewSessionRow {
            id: child,
            owner: "dev",
            title: "child",
            custom_title: false,
            pending_prompt: "",
            agent: "claude-code",
            kind: SessionKind::Work,
            review: ReviewPolicy::Off,
            model: None,
            repo: None,
            grants: &inherited,
        },
    )
    .await
    .unwrap();
    let row = db::get_session(&db, child).await.unwrap().unwrap();
    let expires = row.to_api().grants[0].expires_at.clone().unwrap();
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&expires)
            .unwrap()
            .timestamp(),
        soon.timestamp()
    );
}

/// A `github-user` credential of `owner`'s, stored as adding one would, without asking GitHub.
async fn github_token(
    db: &PgPool,
    credentials: &crate::credentials::Credentials,
    vault: &crate::vault::Vault,
    owner: &str,
    login: &str,
    repositories: &[&str],
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into credentials (id, owner, kind, name, config, secret, is_default) \
         values ($1, $2, 'github-user', $3, $4, $5, false)",
    )
    .bind(id)
    .bind(owner)
    .bind(login)
    .bind(json!({"login": login, "repositories": repositories, "more_repositories": false}))
    .bind(vault.encrypt(b"github_pat_x").unwrap())
    .execute(db)
    .await
    .unwrap();
    assert!(
        credentials
            .github_user(owner, &id.to_string())
            .await
            .unwrap()
            .is_some()
    );
    id
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn an_agent_asks_to_act_as_the_owner_and_the_owner_picks_the_token(db: PgPool) {
    use base64::Engine;
    use sloppod_contracts::ProposalKind;
    use std::sync::Arc;

    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    let vault = crate::vault::Vault::from_base64(&key).unwrap();
    let credentials = Arc::new(
        crate::credentials::Credentials::new(
            db.clone(),
            crate::vault::Vault::from_base64(&key).unwrap(),
        )
        .await
        .unwrap(),
    );
    let kind = crate::proposals::ActAsOwner::new(db.clone(), credentials.clone());
    let id = session(&db, "dev").await;
    let context = || async { db::get_session(&db, id).await.unwrap().unwrap().context() };
    let ask = |repo: &str| json!({"repo": repo, "reason": "the App isn't installed there"});

    let none = kind
        .prepare(&context().await, ask("dev/private"))
        .await
        .unwrap_err();
    assert!(none.0.contains("no GitHub token"), "{}", none.0);

    let work = github_token(&db, &credentials, &vault, "dev", "dev-work", &["org/tools"]).await;
    let mine = github_token(&db, &credentials, &vault, "dev", "dev", &["Dev/Private"]).await;
    github_token(
        &db,
        &credentials,
        &vault,
        "other",
        "other",
        &["dev/private"],
    )
    .await;
    let unreached = kind
        .prepare(&context().await, ask("dev/elsewhere"))
        .await
        .unwrap_err();
    assert!(
        unreached
            .0
            .contains("none of the user's GitHub tokens reach"),
        "{}",
        unreached.0
    );

    // The token that reaches it, under GitHub's name for it.
    let payload = kind
        .prepare(&context().await, ask("dev/private"))
        .await
        .unwrap();
    assert_eq!(payload["repo"], "Dev/Private");
    assert_eq!(payload["credential"], mine.to_string());
    assert_eq!(payload["login"], "dev");

    // The owner picks a token that doesn't reach it: nothing is granted.
    let other = kind
        .edit(&payload, &json!({"credential": work.to_string()}))
        .unwrap();
    assert!(other.get("login").is_none());
    assert!(kind.execute(&context().await, &other).await.is_err());
    assert!(context().await.writable_repo_for("dev/private").is_none());

    // Picking which identity is always a person's call (ADR 28).
    assert!(!kind.standing_approvable());
    let done = kind.execute(&context().await, &payload).await.unwrap();
    assert_eq!(done["login"], "dev");
    let granted = context().await;
    assert_eq!(
        granted.repo_credential("dev/private"),
        Some(mine.to_string().as_str())
    );
    assert_eq!(granted.repo_acting_as("dev/private"), Some("dev"));
    assert!(granted.writable_repo_for("dev/private").is_some());
    let again = kind
        .prepare(&granted, ask("dev/private"))
        .await
        .unwrap_err();
    assert!(again.0.contains("already acts as dev"), "{}", again.0);
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn a_proposal_carries_over_the_review_its_sub_session_s_report_passed(db: PgPool) {
    use sloppod_contracts::ReviewSubject;

    let parent = session(&db, "dev").await;
    let child = session(&db, "dev").await;
    let stranger = session(&db, "dev").await;
    db::set_parent(&db, child, parent).await.unwrap();
    let reviewer = session_of_kind(&db, "dev", SessionKind::Review).await;
    let report = db::insert_proposal(
        &db,
        child,
        "sloppod.session.report",
        ProposalStatus::Executed,
        &json!({"repo": "o/r", "head": "sloppod/p-child", "head_sha": "c0ffee", "review": {"verdict": "pass"}}),
    )
    .await
    .unwrap();
    db::set_review_session(&db, report.id, reviewer)
        .await
        .unwrap();

    let subject = ReviewSubject {
        repo: "O/R".into(),
        base: "main".into(),
        head: "sloppod/p".into(),
        head_sha: "beef".into(),
        title: "Fix".into(),
        body: String::new(),
        describes_change: true,
    };
    let parent_row = db::get_session(&db, parent).await.unwrap().unwrap();
    let reports = |kind: &str| kind == "sloppod.session.report";
    let carried =
        crate::reviews::carried_over_in(&db, &parent_row, &child.to_string(), &subject, reports)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        (carried.reviewer, carried.sha.as_str()),
        (reviewer, "c0ffee")
    );
    assert_eq!(carried.head, "sloppod/p-child");

    // Only a sub-session of the proposer's, and only a review that passed, of the same repository.
    assert!(
        crate::reviews::carried_over_in(&db, &parent_row, &stranger.to_string(), &subject, reports)
            .await
            .is_err()
    );
    let elsewhere = ReviewSubject {
        repo: "o/other".into(),
        ..subject.clone()
    };
    assert!(
        crate::reviews::carried_over_in(&db, &parent_row, &child.to_string(), &elsewhere, reports)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        crate::reviews::carried_over_in(&db, &parent_row, &child.to_string(), &subject, |_| false)
            .await
            .unwrap()
            .is_none(),
        "a kind that doesn't carry its review over"
    );

    // Kept for a day after the report was decided, unless a proposal is in review with it.
    sqlx::query("update proposals set decided_at = now() - interval '2 days' where id = $1")
        .bind(report.id)
        .execute(&db)
        .await
        .unwrap();
    let day_ago = Utc::now() - Duration::hours(24);
    assert!(
        db::idle_review_sessions(&db, day_ago)
            .await
            .unwrap()
            .contains(&reviewer)
    );
    assert!(
        !db::has_active_review(&db, reviewer).await.unwrap(),
        "the report is done"
    );
    let pr = db::insert_proposal(
        &db,
        parent,
        "github.pr.create",
        ProposalStatus::InReview,
        &json!({}),
    )
    .await
    .unwrap();
    assert!(db::claim_reviewer(&db, pr.id, reviewer).await.unwrap());
    assert!(db::has_active_review(&db, reviewer).await.unwrap());
    assert!(
        !db::idle_review_sessions(&db, day_ago)
            .await
            .unwrap()
            .contains(&reviewer)
    );
    // Its verdict is the PR's, not the executed report's, though both name the reviewer.
    assert_eq!(
        db::reviewed_proposal(&db, reviewer)
            .await
            .unwrap()
            .unwrap()
            .id,
        pr.id
    );
    // A second proposal carrying the same review over doesn't get the busy reviewer, and starts afresh.
    let again = db::insert_proposal(
        &db,
        parent,
        "github.pr.create",
        ProposalStatus::InReview,
        &json!({}),
    )
    .await
    .unwrap();
    assert!(!db::claim_reviewer(&db, again.id, reviewer).await.unwrap());
    assert!(
        crate::reviews::carried_over_in(&db, &parent_row, &child.to_string(), &subject, reports)
            .await
            .unwrap()
            .is_none(),
        "a reviewer that's reviewing something isn't offered"
    );
}

#[sqlx::test(migrator = "db::MIGRATOR")]
async fn an_owner_defines_a_capability_grants_it_and_deletes_it(db: PgPool) {
    use crate::definitions::{self, NewDefinition};

    let new = NewDefinition {
        name: "victorialogs".into(),
        description: "Production logs, through VictoriaLogs' MCP server".into(),
        mcp_url: "http://vl-mcp.monitoring.svc:8080/mcp".into(),
        credential_id: None,
    };
    let defined = definitions::create(&db, "dev", &new)
        .await
        .unwrap()
        .unwrap();
    assert!(
        definitions::create(&db, "dev", &new)
            .await
            .unwrap()
            .is_none(),
        "one per name"
    );
    assert!(
        definitions::create(&db, "other", &new)
            .await
            .unwrap()
            .is_some(),
        "per owner"
    );
    assert_eq!(
        definitions::named(&db, "dev", "victorialogs")
            .await
            .unwrap()
            .unwrap()
            .id,
        defined.id
    );
    assert!(
        definitions::named(&db, "nobody", "victorialogs")
            .await
            .unwrap()
            .is_none()
    );

    // Granted, it's the session's under its name, and says what it is.
    let id = session(&db, "dev").await;
    db::grant(&db, id, &definitions::grant(&defined), None)
        .await
        .unwrap();
    let context = db::get_session(&db, id).await.unwrap().unwrap().context();
    assert!(context.has("victorialogs"));
    assert_eq!(
        definitions::servers(&context.grants, "http://127.0.0.1:7777"),
        [(
            "victorialogs".to_owned(),
            "http://127.0.0.1:7777/servers/victorialogs".to_owned()
        )]
    );

    assert!(
        !definitions::delete(&db, "other", defined.id).await.unwrap(),
        "only its owner's"
    );
    assert!(definitions::delete(&db, "dev", defined.id).await.unwrap());
    assert!(
        definitions::named(&db, "dev", "victorialogs")
            .await
            .unwrap()
            .is_none()
    );
}
