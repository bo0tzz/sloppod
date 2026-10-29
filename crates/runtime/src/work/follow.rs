//! Items follow what's linked to them (ADR 30): the runtime moves a link's status as the pull request, issue,
//! session or report it points at moves, and the rules move the item. Every change is recorded with the runtime as its
//! author. Pull requests are followed from the PR watcher (`pulls.rs`), issues and pull requests no watcher covers by
//! polling here until webhooks (#204), sessions from their phase and their reports, and a proposal that opens a pull
//! request links it to the items its session is linked to.

use serde_json::json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::rules::{LinkKind, LinkStatus, Role};
use super::{
    Actor, LINK_COLUMNS, Link, WorkResult, kind_str, lock_item_graph, lock_link, record, settle,
    status_str, write_link_status,
};
use crate::credentials::Credentials;

/// Who the runtime's own changes are by, in an item's history.
pub const RUNTIME: &str = "sloppod";

const BY_RUNTIME: Actor = Actor {
    user: RUNTIME,
    session: None,
};

/// How often linked issues and pull requests are polled.
pub const POLL: std::time::Duration = std::time::Duration::from_secs(60);

/// Moves every one of the owner's links to `kind` `target` to `status`, as the runtime, and the items with them.
/// Returns how many links changed. Targets compare without case, as GitHub's names do.
pub async fn follow(
    db: &PgPool,
    owner: &str,
    kind: LinkKind,
    target: &str,
    status: LinkStatus,
) -> WorkResult<usize> {
    let links: Vec<Uuid> = sqlx::query_scalar(
        "select l.id from work_links l join work_items i on i.id = l.item_id \
         join work_graphs g on g.id = i.graph_id \
         where g.owner = $1 and l.kind = $2 and lower(l.target) = lower($3) and l.status <> $4",
    )
    .bind(owner)
    .bind(kind_str(kind))
    .bind(target)
    .bind(status_str(status))
    .fetch_all(db)
    .await?;
    let mut changed = 0;
    for id in links {
        let mut tx = db.begin().await?;
        let (graph, link) = lock_link(&mut tx, owner, id).await?;
        // It may have moved since it was listed.
        if link.status == status_str(status) {
            continue;
        }
        write_link_status(&mut tx, id, status_str(status)).await?;
        record(
            &mut tx,
            BY_RUNTIME,
            graph,
            Some(link.item_id),
            "link_status",
            Some(json!({"link": id, "status": link.status})),
            Some(json!({"link": id, "status": status})),
            None,
        )
        .await?;
        settle(&mut tx, BY_RUNTIME, graph, &[link.item_id]).await?;
        tx.commit().await?;
        changed += 1;
    }
    Ok(changed)
}

/// A pull request's link status from what GitHub says of it: open, merged (finished), or closed without merging
/// (abandoned, so it stops counting as completing).
pub fn pull_status(state: &str) -> LinkStatus {
    match state.to_ascii_uppercase().as_str() {
        "MERGED" => LinkStatus::Finished,
        "CLOSED" => LinkStatus::Abandoned,
        _ => LinkStatus::Open,
    }
}

/// An issue's (or pull request's) link status from GitHub's REST issue: open; closed as completed (finished); closed
/// as not planned or a duplicate (abandoned). A pull request is finished only if it was merged.
pub fn issue_status(issue: &serde_json::Value) -> LinkStatus {
    if issue["state"] != "closed" {
        return LinkStatus::Open;
    }
    if issue.get("pull_request").is_some_and(|p| !p.is_null()) {
        return match issue["pull_request"]["merged_at"].is_string() {
            true => LinkStatus::Finished,
            false => LinkStatus::Abandoned,
        };
    }
    match issue["state_reason"].as_str() {
        Some("not_planned" | "duplicate") => LinkStatus::Abandoned,
        _ => LinkStatus::Finished,
    }
}

/// Links pull requests a session's proposal opened to the items the session is linked to, each with the role the
/// session's own link has: a pull request from a session that completes an item completes it too.
pub async fn opened(
    db: &PgPool,
    owner: &str,
    session: Uuid,
    repo: &str,
    numbers: &[u64],
) -> WorkResult<()> {
    let items: Vec<(Uuid, String)> = sqlx::query_as(
        "select l.item_id, l.role from work_links l join work_items i on i.id = l.item_id \
         join work_graphs g on g.id = i.graph_id \
         where g.owner = $1 and l.kind = 'session' and l.target = $2",
    )
    .bind(owner)
    .bind(session.to_string())
    .fetch_all(db)
    .await?;
    for (item, role) in items {
        for number in numbers {
            let target = crate::pulls::pull_ref(repo, *number);
            let mut tx = db.begin().await?;
            let graph = lock_item_graph(&mut tx, owner, item).await?;
            if let Some(link) = insert_link(&mut tx, item, &target, &role).await? {
                record(
                    &mut tx,
                    BY_RUNTIME,
                    graph,
                    Some(item),
                    "link",
                    None,
                    Some(json!(link)),
                    None,
                )
                .await?;
                settle(&mut tx, BY_RUNTIME, graph, &[item]).await?;
            }
            tx.commit().await?;
        }
    }
    Ok(())
}

async fn insert_link(
    tx: &mut PgConnection,
    item: Uuid,
    target: &str,
    role: &str,
) -> sqlx::Result<Option<Link>> {
    let role = match role {
        "completes" => Role::Completes,
        _ => Role::Related,
    };
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "insert into work_links (id, item_id, kind, target, role) values ($1, $2, $3, $4, $5) \
         on conflict (item_id, kind, target) do nothing returning {LINK_COLUMNS}"
    )))
    .bind(Uuid::now_v7())
    .bind(item)
    .bind(kind_str(LinkKind::PullRequest))
    .bind(target)
    .bind(super::role_str(role))
    .fetch_optional(&mut *tx)
    .await
}

/// Brings the owner's links to sessions in line with the sessions: open while one acts, finished once it has
/// reported its work done (a sub-session's executed report), abandoned once it's parked, stopped or deleted without
/// having reported. `only` limits it to one session.
pub async fn sessions(db: &PgPool, only: Option<Uuid>) -> WorkResult<usize> {
    let linked: Vec<(String, String, String)> = sqlx::query_as(
        "select distinct g.owner, l.target, \
           case when exists (select 1 from proposals p where p.session_id = s.id \
                             and p.kind = 'sloppod.session.report' and p.status = 'executed') then 'finished' \
                when s.deleted_at is null and s.phase not in ('parked', 'stopped') then 'open' \
                else 'abandoned' end \
         from work_links l join work_items i on i.id = l.item_id join work_graphs g on g.id = i.graph_id \
         join sessions s on s.id::text = l.target and s.owner = g.owner \
         where l.kind = 'session' and ($1::uuid is null or s.id = $1)",
    )
    .bind(only)
    .fetch_all(db)
    .await?;
    let mut changed = 0;
    for (owner, target, status) in linked {
        let status = match status.as_str() {
            "finished" => LinkStatus::Finished,
            "open" => LinkStatus::Open,
            _ => LinkStatus::Abandoned,
        };
        changed += follow(db, &owner, LinkKind::Session, &target, status).await?;
    }
    Ok(changed)
}

/// Polls the issues and pull requests open links point at, as the owner's GitHub App, and follows them.
pub async fn issues(db: &PgPool, credentials: &Credentials) -> WorkResult<usize> {
    let linked: Vec<(String, String, String)> = sqlx::query_as(
        "select distinct g.owner, l.kind, l.target from work_links l \
         join work_items i on i.id = l.item_id join work_graphs g on g.id = i.graph_id \
         where l.kind in ('issue', 'pull_request') and l.status = 'open' order by g.owner, l.target",
    )
    .fetch_all(db)
    .await?;
    let mut changed = 0;
    for (owner, kind, target) in linked {
        let Some((repo, number)) = target
            .rsplit_once('#')
            .and_then(|(repo, n)| Some((repo, n.parse::<u64>().ok()?)))
        else {
            continue;
        };
        let read = async {
            let app = credentials.github(&owner).await?;
            app.issue_state(repo, number).await
        };
        let issue = match read.await {
            Ok(issue) => issue,
            Err(e) => {
                tracing::debug!(%owner, %target, error = %format!("{e:#}"), "reading a linked issue");
                continue;
            }
        };
        let kind = if kind == "issue" {
            LinkKind::Issue
        } else {
            LinkKind::PullRequest
        };
        changed += follow(db, &owner, kind, &target, issue_status(&issue)).await?;
    }
    Ok(changed)
}

/// Follows linked sessions and issues every [`POLL`].
pub async fn run(state: crate::AppState) {
    let mut every = tokio::time::interval(POLL);
    loop {
        every.tick().await;
        if let Err(e) = sessions(&state.db, None).await {
            tracing::warn!(error = ?e, "following linked sessions");
        }
        if let Err(e) = issues(&state.db, &state.credentials).await {
            tracing::warn!(error = ?e, "following linked issues");
        }
    }
}

/// What a carried-out proposal moves: a pull request or stack links what it opened to its session's items, and a
/// sub-session's report finishes its session's links.
pub async fn executed(
    db: &PgPool,
    owner: &str,
    session: Uuid,
    kind: &str,
    payload: &serde_json::Value,
    result: &serde_json::Value,
) -> WorkResult<()> {
    let repo = payload["repo"].as_str().unwrap_or_default();
    match kind {
        "github.pr.create" => {
            if let Some(number) = result["number"].as_u64() {
                opened(db, owner, session, repo, &[number]).await?;
            }
        }
        "github.stack.create" => {
            let numbers: Vec<u64> = result["pull_requests"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|pull| pull["number"].as_u64())
                .collect();
            opened(db, owner, session, repo, &numbers).await?;
        }
        "sloppod.session.report" => {
            sessions(db, Some(session)).await?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sloppod_protocol::api::{ReviewPolicy, SessionKind};

    use super::*;
    use crate::work::rules::State;
    use crate::work::{NewItem, NewLink, create_graph, create_item, item, item_history};

    const DEV: Actor = Actor {
        user: "dev",
        session: None,
    };

    async fn new_item(db: &PgPool, graph: Uuid, title: &str) -> Uuid {
        let new = NewItem {
            title: title.into(),
            description: String::new(),
            parent_id: None,
            after: None,
        };
        create_item(db, DEV, graph, &new).await.unwrap().id
    }

    async fn link(db: &PgPool, item: Uuid, kind: LinkKind, target: &str, role: Role) {
        let new = NewLink {
            kind,
            target: target.into(),
            role,
        };
        crate::work::add_link(db, DEV, item, &new).await.unwrap();
    }

    async fn state(db: &PgPool, id: Uuid) -> String {
        item(db, "dev", id).await.unwrap().unwrap().state
    }

    async fn session(db: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        crate::db::insert_session(
            db,
            crate::db::NewSessionRow {
                id,
                owner: "dev",
                title: "t",
                custom_title: false,
                pending_prompt: "",
                agent: "claude-code",
                kind: SessionKind::Work,
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

    #[test]
    fn reads_what_github_says_of_a_link() {
        assert_eq!(pull_status("MERGED"), LinkStatus::Finished);
        assert_eq!(pull_status("CLOSED"), LinkStatus::Abandoned);
        assert_eq!(pull_status("OPEN"), LinkStatus::Open);
        let issue = |v: serde_json::Value| issue_status(&v);
        assert_eq!(issue(json!({"state": "open"})), LinkStatus::Open);
        assert_eq!(
            issue(json!({"state": "closed", "state_reason": "completed"})),
            LinkStatus::Finished
        );
        assert_eq!(
            issue(json!({"state": "closed", "state_reason": "not_planned"})),
            LinkStatus::Abandoned
        );
        assert_eq!(
            issue(json!({"state": "closed", "pull_request": {"merged_at": null}})),
            LinkStatus::Abandoned
        );
        assert_eq!(
            issue(
                json!({"state": "closed", "pull_request": {"merged_at": "2026-10-29T12:00:00Z"}})
            ),
            LinkStatus::Finished
        );
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_merged_pull_request_finishes_its_item_as_the_runtime(db: PgPool) {
        let graph = create_graph(&db, "dev", "g").await.unwrap().id;
        let fix = new_item(&db, graph, "fix").await;
        link(&db, fix, LinkKind::PullRequest, "o/r#7", Role::Completes).await;
        assert_eq!(state(&db, fix).await, "active");
        // Someone else's link to the same pull request isn't theirs to move.
        let theirs = create_graph(&db, "eve", "g").await.unwrap().id;
        let eve = Actor {
            user: "eve",
            session: None,
        };
        let other = create_item(
            &db,
            eve,
            theirs,
            &NewItem {
                title: "x".into(),
                description: String::new(),
                parent_id: None,
                after: None,
            },
        )
        .await
        .unwrap()
        .id;
        let new = NewLink {
            kind: LinkKind::PullRequest,
            target: "o/r#7".into(),
            role: Role::Completes,
        };
        crate::work::add_link(&db, eve, other, &new).await.unwrap();

        let moved = follow(
            &db,
            "dev",
            LinkKind::PullRequest,
            "O/R#7",
            LinkStatus::Finished,
        )
        .await
        .unwrap();
        assert_eq!(moved, 1);
        assert_eq!(state(&db, fix).await, "done");
        assert_eq!(
            item(&db, "eve", other).await.unwrap().unwrap().state,
            "active"
        );
        // Following again changes nothing.
        assert_eq!(
            follow(
                &db,
                "dev",
                LinkKind::PullRequest,
                "o/r#7",
                LinkStatus::Finished
            )
            .await
            .unwrap(),
            0
        );
        let history = item_history(&db, "dev", fix).await.unwrap().unwrap();
        let by_runtime: Vec<&str> = history
            .iter()
            .filter(|e| e.actor == RUNTIME)
            .map(|e| e.kind.as_str())
            .collect();
        assert_eq!(by_runtime, ["link_status", "state"]);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_proposal_s_pull_request_joins_its_session_s_items(db: PgPool) {
        let graph = create_graph(&db, "dev", "g").await.unwrap().id;
        let worker = session(&db).await;
        let fix = new_item(&db, graph, "fix").await;
        let background = new_item(&db, graph, "background").await;
        link(
            &db,
            fix,
            LinkKind::Session,
            &worker.to_string(),
            Role::Completes,
        )
        .await;
        link(
            &db,
            background,
            LinkKind::Session,
            &worker.to_string(),
            Role::Related,
        )
        .await;
        executed(
            &db,
            "dev",
            worker,
            "github.pr.create",
            &json!({"repo": "o/r"}),
            &json!({"number": 9, "url": "https://github.com/o/r/pull/9"}),
        )
        .await
        .unwrap();
        let links = |id| {
            let db = db.clone();
            async move { item(&db, "dev", id).await.unwrap().unwrap().links }
        };
        let pr = |links: Vec<crate::work::Link>| {
            links
                .into_iter()
                .find(|l| l.kind == "pull_request")
                .map(|l| (l.target, l.role))
        };
        assert_eq!(
            pr(links(fix).await),
            Some(("o/r#9".into(), "completes".into()))
        );
        assert_eq!(
            pr(links(background).await),
            Some(("o/r#9".into(), "related".into()))
        );
        // Its merge finishes the item it completes, and only that one.
        follow(
            &db,
            "dev",
            LinkKind::PullRequest,
            "o/r#9",
            LinkStatus::Finished,
        )
        .await
        .unwrap();
        assert_eq!(
            state(&db, fix).await,
            "active",
            "the session still works on it"
        );
        assert_eq!(state(&db, background).await, "active");

        // A stack links each of its pull requests; carrying one out again adds nothing twice.
        let stack = json!({"pull_requests": [{"number": 10}, {"number": 11}]});
        for _ in 0..2 {
            executed(
                &db,
                "dev",
                worker,
                "github.stack.create",
                &json!({"repo": "o/r"}),
                &stack,
            )
            .await
            .unwrap();
        }
        assert_eq!(links(background).await.len(), 4);
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn session_links_follow_the_session(db: PgPool) {
        let graph = create_graph(&db, "dev", "g").await.unwrap().id;
        let worker = session(&db).await;
        let fix = new_item(&db, graph, "fix").await;
        link(
            &db,
            fix,
            LinkKind::Session,
            &worker.to_string(),
            Role::Completes,
        )
        .await;
        sessions(&db, None).await.unwrap();
        assert_eq!(state(&db, fix).await, "active");
        // Parked without reporting: nothing works on it, and it doesn't finish it.
        sqlx::query("update sessions set phase = 'parked' where id = $1")
            .bind(worker)
            .execute(&db)
            .await
            .unwrap();
        sessions(&db, None).await.unwrap();
        assert_eq!(state(&db, fix).await, "open");
        // Woken, then reported done.
        sqlx::query("update sessions set phase = 'running' where id = $1")
            .bind(worker)
            .execute(&db)
            .await
            .unwrap();
        sessions(&db, None).await.unwrap();
        assert_eq!(state(&db, fix).await, "active");
        let report = crate::db::insert_proposal(
            &db,
            worker,
            "sloppod.session.report",
            sloppod_protocol::api::ProposalStatus::Pending,
            &json!({"summary": "done"}),
        )
        .await
        .unwrap();
        sqlx::query("update proposals set status = 'executed' where id = $1")
            .bind(report.id)
            .execute(&db)
            .await
            .unwrap();
        executed(
            &db,
            "dev",
            worker,
            "sloppod.session.report",
            &json!({}),
            &json!({}),
        )
        .await
        .unwrap();
        assert_eq!(state(&db, fix).await, State::Done.as_str());
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn linked_issues_and_pull_requests_are_polled(db: PgPool) {
        use crate::credentials::GITHUB_APP;

        let key = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [7u8; 32]);
        let credentials =
            Credentials::new(db.clone(), crate::vault::Vault::from_base64(&key).unwrap())
                .await
                .unwrap();
        let (_, app) = crate::github::tests::fake_github().await;
        credentials
            .add_github_client("dev", GITHUB_APP, json!({}), app)
            .await;
        let graph = create_graph(&db, "dev", "g").await.unwrap().id;
        let mut items = Vec::new();
        for (n, kind) in [
            (1, LinkKind::Issue),
            (3, LinkKind::Issue),
            (4, LinkKind::Issue),
            (5, LinkKind::PullRequest),
            (6, LinkKind::PullRequest),
        ] {
            let id = new_item(&db, graph, &format!("#{n}")).await;
            link(&db, id, kind, &format!("o/r#{n}"), Role::Completes).await;
            items.push(id);
        }
        assert_eq!(issues(&db, &credentials).await.unwrap(), 4);
        let mut states = Vec::new();
        for id in &items {
            states.push(state(&db, *id).await);
        }
        // Open; closed as completed; as not planned (it stops counting); merged; closed unmerged.
        assert_eq!(states, ["open", "done", "open", "done", "open"]);
        // What's no longer open isn't asked about again.
        assert_eq!(issues(&db, &credentials).await.unwrap(), 0);
    }
}
