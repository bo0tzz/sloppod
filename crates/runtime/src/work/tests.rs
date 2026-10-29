use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::rules::{LinkKind, LinkStatus, Role, State};
use super::*;

const DEV: Actor = Actor {
    user: "dev",
    session: None,
};

fn new_item(title: &str, parent: Option<Uuid>) -> NewItem {
    NewItem {
        title: title.into(),
        description: String::new(),
        parent_id: parent,
        after: None,
    }
}

async fn add(db: &PgPool, graph: Uuid, title: &str, parent: Option<Uuid>) -> Item {
    create_item(db, DEV, graph, &new_item(title, parent))
        .await
        .unwrap()
}

async fn state(db: &PgPool, id: Uuid) -> String {
    item(db, "dev", id).await.unwrap().unwrap().state
}

async fn ready(db: &PgPool, graph: Uuid) -> Vec<String> {
    frontier(db, "dev", graph)
        .await
        .unwrap()
        .unwrap()
        .into_iter()
        .map(|item| item.title)
        .collect()
}

fn pull(target: &str) -> NewLink {
    NewLink {
        kind: LinkKind::PullRequest,
        target: target.into(),
        role: Role::Completes,
    }
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn graphs_are_their_owners_own(db: PgPool) {
    let graph = create_graph(&db, "dev", " sloppod ").await.unwrap();
    assert_eq!(graph.name, "sloppod");
    assert!(matches!(
        create_graph(&db, "dev", "sloppod").await,
        Err(WorkError::Conflict(_))
    ));
    assert!(matches!(
        create_graph(&db, "dev", " ").await,
        Err(WorkError::Invalid(_))
    ));
    // Someone else has their own namespace, and can't see or change this one.
    create_graph(&db, "eve", "sloppod").await.unwrap();
    assert!(super::graph(&db, "eve", graph.id).await.unwrap().is_none());
    let eve = Actor {
        user: "eve",
        session: None,
    };
    assert!(matches!(
        create_item(&db, eve, graph.id, &new_item("x", None)).await,
        Err(WorkError::NotFound)
    ));
    let item = add(&db, graph.id, "mine", None).await;
    assert!(super::item(&db, "eve", item.id).await.unwrap().is_none());
    assert!(matches!(
        edit_item(
            &db,
            eve,
            item.id,
            &ItemEdit {
                title: Some("x".into()),
                description: None
            }
        )
        .await,
        Err(WorkError::NotFound)
    ));
    assert!(!delete_graph(&db, "eve", graph.id).await.unwrap());

    let renamed = rename_graph(&db, DEV, graph.id, "work").await.unwrap();
    assert_eq!(renamed.name, "work");
    assert!(delete_graph(&db, "dev", graph.id).await.unwrap());
    assert!(super::item(&db, "dev", item.id).await.unwrap().is_none());
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn the_frontier_follows_blocks_parts_and_ranks(db: PgPool) {
    let graph = create_graph(&db, "dev", "g").await.unwrap().id;
    let design = add(&db, graph, "design", None).await;
    let build = add(&db, graph, "build", None).await;
    // Ranked first, though added last.
    create_item(
        &db,
        DEV,
        graph,
        &NewItem {
            after: Some(None),
            ..new_item("first", None)
        },
    )
    .await
    .unwrap();
    assert_eq!(ready(&db, graph).await, ["first", "design", "build"]);

    add_block(&db, DEV, design.id, build.id).await.unwrap();
    assert_eq!(ready(&db, graph).await, ["first", "design"]);
    // No cycles, through blocks or parts.
    assert!(matches!(
        add_block(&db, DEV, build.id, design.id).await,
        Err(WorkError::Conflict(_))
    ));
    let part = add(&db, graph, "part", Some(build.id)).await;
    assert!(matches!(
        add_block(&db, DEV, build.id, part.id).await,
        Err(WorkError::Conflict(_))
    ));
    let to_part = ItemMove {
        parent_id: Some(Some(part.id)),
        after: None,
    };
    assert!(matches!(
        move_item(&db, DEV, build.id, &to_part).await,
        Err(WorkError::Conflict(_))
    ));
    // Not across graphs.
    let other = create_graph(&db, "dev", "other").await.unwrap().id;
    let elsewhere = add(&db, other, "elsewhere", None).await;
    assert!(matches!(
        add_block(&db, DEV, design.id, elsewhere.id).await,
        Err(WorkError::Invalid(_))
    ));

    // `part` waits on what blocks `build`, its parent; `build` waits on `part`.
    assert_eq!(ready(&db, graph).await, ["first", "design"]);
    move_state(&db, DEV, design.id, State::Done).await.unwrap();
    assert_eq!(ready(&db, graph).await, ["first", "part"]);
    // Moved first among the top level.
    let to_front = ItemMove {
        parent_id: None,
        after: Some(None),
    };
    move_item(
        &db,
        DEV,
        part.id,
        &ItemMove {
            parent_id: Some(None),
            ..to_front
        },
    )
    .await
    .unwrap();
    assert_eq!(ready(&db, graph).await, ["part", "first", "build"]);
    remove_block(&db, DEV, design.id, build.id).await.unwrap();
    let item = super::item(&db, "dev", build.id).await.unwrap().unwrap();
    assert!(item.blocked_by.is_empty());
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn links_and_parts_move_items(db: PgPool) {
    let graph = create_graph(&db, "dev", "g").await.unwrap().id;
    let epic = add(&db, graph, "epic", None).await;
    let fix = add(&db, graph, "fix", Some(epic.id)).await;
    let docs = add(&db, graph, "docs", Some(epic.id)).await;

    // A completing pull request: open makes it active, merged makes it done.
    let link = add_link(&db, DEV, fix.id, &pull("o/r#1")).await.unwrap();
    assert_eq!(state(&db, fix.id).await, "active");
    assert!(matches!(
        add_link(&db, DEV, fix.id, &pull("o/r#1")).await,
        Err(WorkError::Conflict(_))
    ));
    assert!(matches!(
        add_link(&db, DEV, fix.id, &pull("not a pull request")).await,
        Err(WorkError::Invalid(_))
    ));
    let nobodys = NewLink {
        kind: LinkKind::Session,
        target: Uuid::now_v7().to_string(),
        role: Role::Related,
    };
    assert!(matches!(
        add_link(&db, DEV, fix.id, &nobodys).await,
        Err(WorkError::Invalid(_))
    ));
    // Not done by hand while its link decides; the parent not while a part is open.
    assert!(matches!(
        move_state(&db, DEV, fix.id, State::Done).await,
        Err(WorkError::Conflict(_))
    ));
    assert!(matches!(
        move_state(&db, DEV, epic.id, State::Done).await,
        Err(WorkError::Conflict(_))
    ));
    set_link_status(&db, DEV, link.id, LinkStatus::Finished)
        .await
        .unwrap();
    assert_eq!(state(&db, fix.id).await, "done");
    assert_eq!(state(&db, epic.id).await, "open");
    // Its last part dropped, the parent is done; a new part opens it again.
    move_state(&db, DEV, docs.id, State::Dropped).await.unwrap();
    assert_eq!(state(&db, epic.id).await, "done");
    let more = add(&db, graph, "more", Some(epic.id)).await;
    assert_eq!(state(&db, epic.id).await, "open");
    move_state(&db, DEV, more.id, State::Done).await.unwrap();
    assert_eq!(state(&db, epic.id).await, "done");

    // A pull request closed unmerged stops counting, rather than finishing the item.
    let retry = add(&db, graph, "retry", None).await;
    let closed = add_link(&db, DEV, retry.id, &pull("o/r#2")).await.unwrap();
    set_link_status(&db, DEV, closed.id, LinkStatus::Abandoned)
        .await
        .unwrap();
    assert_eq!(state(&db, retry.id).await, "open");
    move_state(&db, DEV, retry.id, State::Done).await.unwrap();
    assert_eq!(state(&db, retry.id).await, "done");
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn changes_are_recorded_and_undone_by_their_inverse(db: PgPool) {
    let graph = create_graph(&db, "dev", "g").await.unwrap().id;
    let item = add(&db, graph, "draft", None).await;
    let edit = |title: &str| ItemEdit {
        title: Some(title.into()),
        description: None,
    };
    edit_item(&db, DEV, item.id, &edit("better")).await.unwrap();
    let history = item_history(&db, "dev", item.id).await.unwrap().unwrap();
    assert_eq!(
        history.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
        ["create", "edit"]
    );
    let edited = &history[1];
    assert_eq!(
        edited.before,
        Some(json!({"title": "draft", "description": ""}))
    );
    assert_eq!(edited.actor, "dev");

    let inverse = undo(&db, DEV, edited.id).await.unwrap();
    assert_eq!(inverse.undoes, Some(edited.id));
    assert_eq!(
        super::item(&db, "dev", item.id)
            .await
            .unwrap()
            .unwrap()
            .title,
        "draft"
    );
    assert!(matches!(
        undo(&db, DEV, edited.id).await,
        Err(WorkError::Conflict(_))
    ));
    // Undoing the undo redoes it.
    undo(&db, DEV, inverse.id).await.unwrap();
    assert_eq!(
        super::item(&db, "dev", item.id)
            .await
            .unwrap()
            .unwrap()
            .title,
        "better"
    );
    // A change that a later one overwrote isn't undone over it.
    edit_item(&db, DEV, item.id, &edit("best")).await.unwrap();
    let history = item_history(&db, "dev", item.id).await.unwrap().unwrap();
    let redone = history.iter().rev().nth(1).unwrap();
    assert!(matches!(
        undo(&db, DEV, redone.id).await,
        Err(WorkError::Conflict(_))
    ));

    // Edges and links come back as they were.
    let other = add(&db, graph, "other", None).await;
    add_block(&db, DEV, other.id, item.id).await.unwrap();
    remove_block(&db, DEV, other.id, item.id).await.unwrap();
    let link = add_link(&db, DEV, item.id, &pull("o/r#3")).await.unwrap();
    remove_link(&db, DEV, link.id).await.unwrap();
    let history = item_history(&db, "dev", item.id).await.unwrap().unwrap();
    for kind in ["unblock", "unlink"] {
        let event = history.iter().find(|e| e.kind == kind).unwrap();
        undo(&db, DEV, event.id).await.unwrap();
    }
    let restored = super::item(&db, "dev", item.id).await.unwrap().unwrap();
    assert_eq!(restored.blocked_by, [other.id]);
    assert_eq!(restored.links, [link]);
    assert_eq!(restored.state, "active");
    // The blocker's history shows the edge too.
    let blocker = item_history(&db, "dev", other.id).await.unwrap().unwrap();
    assert!(blocker.iter().any(|e| e.kind == "block"));

    // The rules' own changes aren't undone; the link that caused them is what to change.
    let automatic = item_history(&db, "dev", item.id)
        .await
        .unwrap()
        .unwrap()
        .into_iter()
        .find(|e| e.automatic)
        .unwrap();
    assert!(matches!(
        undo(&db, DEV, automatic.id).await,
        Err(WorkError::Conflict(_))
    ));

    // Undoing a creation drops the item and keeps its history.
    let created = item_history(&db, "dev", other.id).await.unwrap().unwrap()[0].clone();
    undo(&db, DEV, created.id).await.unwrap();
    assert_eq!(state(&db, other.id).await, "dropped");
    // Someone else can't undo the owner's changes.
    let eve = Actor {
        user: "eve",
        session: None,
    };
    assert!(matches!(
        undo(&db, eve, created.id).await,
        Err(WorkError::NotFound)
    ));
    assert!(
        graph_history(&db, "dev", graph)
            .await
            .unwrap()
            .unwrap()
            .len()
            > 10
    );
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn what_the_rules_finished_reopens_when_its_cause_goes(db: PgPool) {
    let graph = create_graph(&db, "dev", "g").await.unwrap().id;
    let fix = add(&db, graph, "fix", None).await;
    let finish = |link: Uuid| set_link_status(&db, DEV, link, LinkStatus::Finished);

    // Its completing link removed, or abandoned.
    let link = add_link(&db, DEV, fix.id, &pull("o/r#1")).await.unwrap();
    finish(link.id).await.unwrap();
    assert_eq!(state(&db, fix.id).await, "done");
    remove_link(&db, DEV, link.id).await.unwrap();
    assert_eq!(state(&db, fix.id).await, "open");
    let link = add_link(&db, DEV, fix.id, &pull("o/r#2")).await.unwrap();
    finish(link.id).await.unwrap();
    set_link_status(&db, DEV, link.id, LinkStatus::Abandoned)
        .await
        .unwrap();
    assert_eq!(state(&db, fix.id).await, "open");

    // Its last part moved away.
    let epic = add(&db, graph, "epic", None).await;
    let part = add(&db, graph, "part", Some(epic.id)).await;
    move_state(&db, DEV, part.id, State::Done).await.unwrap();
    assert_eq!(state(&db, epic.id).await, "done");
    let out = ItemMove {
        parent_id: Some(None),
        after: None,
    };
    move_item(&db, DEV, part.id, &out).await.unwrap();
    assert_eq!(state(&db, epic.id).await, "open");

    // Done by hand, with nothing linked, it stays done.
    move_state(&db, DEV, epic.id, State::Done).await.unwrap();
    let related = NewLink {
        role: Role::Related,
        ..pull("o/r#3")
    };
    let link = add_link(&db, DEV, epic.id, &related).await.unwrap();
    remove_link(&db, DEV, link.id).await.unwrap();
    assert_eq!(state(&db, epic.id).await, "done");

    // A part can't block what it's part of: it waits on what blocks its parent.
    let child = add(&db, graph, "child", Some(fix.id)).await;
    assert!(matches!(
        add_block(&db, DEV, child.id, fix.id).await,
        Err(WorkError::Conflict(_))
    ));
}

#[sqlx::test(migrator = "crate::db::MIGRATOR")]
async fn a_link_whose_status_changed_isnt_unmade_over_it(db: PgPool) {
    let graph = create_graph(&db, "dev", "g").await.unwrap().id;
    let item = add(&db, graph, "item", None).await;
    let link = add_link(&db, DEV, item.id, &pull("o/r#1")).await.unwrap();
    set_link_status(&db, DEV, link.id, LinkStatus::Finished)
        .await
        .unwrap();
    let history = item_history(&db, "dev", item.id).await.unwrap().unwrap();
    let linked = history.iter().find(|e| e.kind == "link").unwrap();
    assert!(matches!(
        undo(&db, DEV, linked.id).await,
        Err(WorkError::Conflict(_))
    ));
    // Undoing the status change first, then the link, works.
    let changed = history.iter().find(|e| e.kind == "link_status").unwrap();
    undo(&db, DEV, changed.id).await.unwrap();
    undo(&db, DEV, linked.id).await.unwrap();
    assert!(
        super::item(&db, "dev", item.id)
            .await
            .unwrap()
            .unwrap()
            .links
            .is_empty()
    );
}
