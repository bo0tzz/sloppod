//! Work graphs (ADR 30): an owner's items of work, what blocks what, the links that carry each item out, and every
//! change as an event that can be undone. The rules live in [`rules`]; this keeps the graphs in the database and applies
//! the rules to every change, under a lock on the graph so changes to one graph happen one at a time.

pub mod agent;
pub mod api;
pub mod rules;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use rules::{LinkKind, LinkStatus, Role, State};

#[derive(Debug)]
pub enum WorkError {
    NotFound,
    Invalid(String),
    Conflict(String),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for WorkError {
    fn from(e: sqlx::Error) -> Self {
        WorkError::Db(e)
    }
}

pub type WorkResult<T> = Result<T, WorkError>;

fn invalid<T>(message: impl Into<String>) -> WorkResult<T> {
    Err(WorkError::Invalid(message.into()))
}

fn conflict<T>(message: impl Into<String>) -> WorkResult<T> {
    Err(WorkError::Conflict(message.into()))
}

/// Who makes a change: the owner, or (later) one of their sessions.
#[derive(Debug, Clone, Copy)]
pub struct Actor<'a> {
    pub user: &'a str,
    pub session: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct WorkGraph {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Item {
    pub id: Uuid,
    pub graph_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub title: String,
    pub description: String,
    pub state: String,
    pub rank: f64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// What it links to.
    #[sqlx(skip)]
    pub links: Vec<Link>,
    /// The items it blocks.
    #[sqlx(skip)]
    pub blocks: Vec<Uuid>,
    /// The items that block it.
    #[sqlx(skip)]
    pub blocked_by: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Link {
    pub id: Uuid,
    pub item_id: Uuid,
    pub kind: String,
    pub target: String,
    pub role: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Event {
    pub id: Uuid,
    pub graph_id: Uuid,
    pub item_id: Option<Uuid>,
    pub actor: String,
    pub session_id: Option<Uuid>,
    pub kind: String,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub automatic: bool,
    pub undoes: Option<Uuid>,
    pub at: DateTime<Utc>,
}

/// A field that can be left out, set to null, or set: `None`, `Some(None)`, `Some(Some(_))`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Deserialize)]
pub struct NewItem {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// The sibling it goes right after; null for first, left out for last.
    #[serde(default, deserialize_with = "present")]
    pub after: Option<Option<Uuid>>,
}

#[derive(Debug, Deserialize)]
pub struct ItemEdit {
    pub title: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ItemMove {
    /// The item it becomes part of; null for none, left out to stay where it is.
    #[serde(default, deserialize_with = "present")]
    pub parent_id: Option<Option<Uuid>>,
    /// The sibling it goes right after; null for first, left out for last.
    #[serde(default, deserialize_with = "present")]
    pub after: Option<Option<Uuid>>,
}

#[derive(Debug, Deserialize)]
pub struct NewLink {
    pub kind: LinkKind,
    pub target: String,
    pub role: Role,
}

fn kind_str(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Issue => "issue",
        LinkKind::PullRequest => "pull_request",
        LinkKind::Session => "session",
        LinkKind::Proposal => "proposal",
    }
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::Completes => "completes",
        Role::Related => "related",
    }
}

fn status_str(status: LinkStatus) -> &'static str {
    match status {
        LinkStatus::Open => "open",
        LinkStatus::Finished => "finished",
        LinkStatus::Abandoned => "abandoned",
    }
}

fn parse<T: for<'de> Deserialize<'de>>(s: &str) -> Option<T> {
    serde_json::from_value(Value::String(s.to_owned())).ok()
}

fn check_title(title: &str) -> WorkResult<()> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return invalid("a title is 1 to 200 characters");
    }
    Ok(())
}

fn check_description(description: &str) -> WorkResult<()> {
    if description.chars().count() > 20_000 {
        return invalid(
            "a description is at most 20000 characters; the details belong in a linked issue",
        );
    }
    Ok(())
}

fn check_graph_name(name: &str) -> WorkResult<()> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return invalid("a graph's name is 1 to 100 characters");
    }
    Ok(())
}

/// Where a link points, checked: well-formed, and a session or proposal the owner's own. Ids come back in their
/// canonical form, so a link is found by the id's text however it was written.
async fn check_target(
    tx: &mut PgConnection,
    owner: &str,
    kind: LinkKind,
    target: &str,
) -> WorkResult<String> {
    match kind {
        LinkKind::Issue | LinkKind::PullRequest => {
            let well_formed = target.rsplit_once('#').is_some_and(|(repo, number)| {
                sloppod_contracts::is_repo_name(repo) && number.parse::<u64>().is_ok_and(|n| n > 0)
            });
            if !well_formed {
                return invalid("an issue or pull request is named owner/repo#number");
            }
        }
        LinkKind::Session | LinkKind::Proposal => {
            let Ok(id) = target.parse::<Uuid>() else {
                return invalid("a session or proposal is named by its id");
            };
            let query = if kind == LinkKind::Session {
                "select exists (select 1 from sessions where id = $1 and owner = $2)"
            } else {
                "select exists (select 1 from proposals p join sessions s on s.id = p.session_id \
                 where p.id = $1 and s.owner = $2)"
            };
            let exists: bool = sqlx::query_scalar(query)
                .bind(id)
                .bind(owner)
                .fetch_one(&mut *tx)
                .await?;
            if !exists {
                return invalid(format!("no {} of yours has that id", kind_str(kind)));
            }
            return Ok(id.to_string());
        }
    }
    Ok(target.to_owned())
}

pub async fn list_graphs(db: &PgPool, owner: &str) -> sqlx::Result<Vec<WorkGraph>> {
    sqlx::query_as("select id, name, created_at from work_graphs where owner = $1 order by name")
        .bind(owner)
        .fetch_all(db)
        .await
}

pub async fn graph(db: &PgPool, owner: &str, id: Uuid) -> sqlx::Result<Option<WorkGraph>> {
    sqlx::query_as("select id, name, created_at from work_graphs where id = $1 and owner = $2")
        .bind(id)
        .bind(owner)
        .fetch_optional(db)
        .await
}

pub async fn create_graph(db: &PgPool, owner: &str, name: &str) -> WorkResult<WorkGraph> {
    check_graph_name(name)?;
    let created: Option<WorkGraph> = sqlx::query_as(
        "insert into work_graphs (id, owner, name) values ($1, $2, $3) \
         on conflict (owner, name) do nothing returning id, name, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(owner)
    .bind(name.trim())
    .fetch_optional(db)
    .await?;
    match created {
        Some(graph) => Ok(graph),
        None => conflict(format!("you have a graph called {} already", name.trim())),
    }
}

/// Deletes a graph with everything in it, its history included.
pub async fn delete_graph(db: &PgPool, owner: &str, id: Uuid) -> sqlx::Result<bool> {
    Ok(
        sqlx::query("delete from work_graphs where id = $1 and owner = $2")
            .bind(id)
            .bind(owner)
            .execute(db)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn rename_graph(
    db: &PgPool,
    actor: Actor<'_>,
    id: Uuid,
    name: &str,
) -> WorkResult<WorkGraph> {
    check_graph_name(name)?;
    let mut tx = db.begin().await?;
    lock_graph(&mut tx, actor.user, id).await?;
    let before: String = sqlx::query_scalar("select name from work_graphs where id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    set_graph_name(&mut tx, id, name.trim()).await?;
    record(
        &mut tx,
        actor,
        id,
        None,
        "rename",
        Some(json!({"name": before})),
        Some(json!({"name": name.trim()})),
        None,
    )
    .await?;
    tx.commit().await?;
    graph(db, actor.user, id).await?.ok_or(WorkError::NotFound)
}

async fn set_graph_name(tx: &mut PgConnection, id: Uuid, name: &str) -> WorkResult<()> {
    let taken: bool = sqlx::query_scalar(
        "select exists (select 1 from work_graphs a join work_graphs b on a.owner = b.owner \
         where a.id = $1 and b.id <> $1 and b.name = $2)",
    )
    .bind(id)
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return conflict(format!("you have a graph called {name} already"));
    }
    sqlx::query("update work_graphs set name = $2 where id = $1")
        .bind(id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Locks one of the owner's graphs for a change.
async fn lock_graph(tx: &mut PgConnection, owner: &str, graph: Uuid) -> WorkResult<()> {
    let found: Option<Uuid> =
        sqlx::query_scalar("select id from work_graphs where id = $1 and owner = $2 for update")
            .bind(graph)
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?;
    found.map(|_| ()).ok_or(WorkError::NotFound)
}

/// Locks the graph one of the owner's items is in, and returns it.
async fn lock_item_graph(tx: &mut PgConnection, owner: &str, item: Uuid) -> WorkResult<Uuid> {
    let graph: Option<Uuid> = sqlx::query_scalar(
        "select g.id from work_items i join work_graphs g on g.id = i.graph_id \
         where i.id = $1 and g.owner = $2 for update of g",
    )
    .bind(item)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    graph.ok_or(WorkError::NotFound)
}

/// The graph as the rules see it.
async fn snapshot(tx: &mut PgConnection, graph: Uuid) -> sqlx::Result<rules::Graph> {
    let items: Vec<(Uuid, Option<Uuid>, String, f64, bool)> = sqlx::query_as(
        "select id, parent_id, state, rank, state_by_rules from work_items where graph_id = $1",
    )
    .bind(graph)
    .fetch_all(&mut *tx)
    .await?;
    let blocks: Vec<(Uuid, Uuid)> =
        sqlx::query_as("select blocker_id, blocked_id from work_edges where graph_id = $1")
            .bind(graph)
            .fetch_all(&mut *tx)
            .await?;
    let links: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "select l.item_id, l.kind, l.role, l.status from work_links l \
         join work_items i on i.id = l.item_id where i.graph_id = $1",
    )
    .bind(graph)
    .fetch_all(&mut *tx)
    .await?;
    Ok(rules::Graph::new(
        items
            .into_iter()
            .map(|(id, parent, state, rank, automatic)| rules::Item {
                id,
                parent,
                state: State::parse(&state).unwrap_or(State::Open),
                rank,
                automatic,
            })
            .collect(),
        blocks,
        links
            .into_iter()
            .filter_map(|(item, kind, role, status)| {
                Some(rules::Link {
                    item,
                    kind: parse(&kind)?,
                    role: parse(&role)?,
                    status: parse(&status)?,
                })
            })
            .collect(),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn record(
    tx: &mut PgConnection,
    actor: Actor<'_>,
    graph: Uuid,
    item: Option<Uuid>,
    kind: &str,
    before: Option<Value>,
    after: Option<Value>,
    undoes: Option<Uuid>,
) -> sqlx::Result<Event> {
    record_as(tx, actor, graph, item, kind, before, after, undoes, false).await
}

#[allow(clippy::too_many_arguments)]
async fn record_as(
    tx: &mut PgConnection,
    actor: Actor<'_>,
    graph: Uuid,
    item: Option<Uuid>,
    kind: &str,
    before: Option<Value>,
    after: Option<Value>,
    undoes: Option<Uuid>,
    automatic: bool,
) -> sqlx::Result<Event> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "insert into work_events (id, graph_id, item_id, actor, session_id, kind, before, after, undoes, automatic) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) returning {EVENT_COLUMNS}"
    )))
    .bind(Uuid::now_v7())
    .bind(graph)
    .bind(item)
    .bind(actor.user)
    .bind(actor.session)
    .bind(kind)
    .bind(before)
    .bind(after)
    .bind(undoes)
    .bind(automatic)
    .fetch_one(&mut *tx)
    .await
}

const EVENT_COLUMNS: &str =
    "id, graph_id, item_id, actor, session_id, kind, before, after, automatic, undoes, at";

/// Sets an item's state, by the rules or by hand.
async fn set_state(
    tx: &mut PgConnection,
    item: Uuid,
    state: State,
    by_rules: bool,
) -> sqlx::Result<()> {
    sqlx::query(
        "update work_items set state = $2, state_by_rules = $3, updated_at = now() where id = $1",
    )
    .bind(item)
    .bind(state.as_str())
    .bind(by_rules)
    .execute(&mut *tx)
    .await
    .map(|_| ())
}

/// Brings the items in `from`, and the items they're part of, into the state the rules put them in, recording each
/// change as automatic.
async fn settle(
    tx: &mut PgConnection,
    actor: Actor<'_>,
    graph: Uuid,
    from: &[Uuid],
) -> sqlx::Result<()> {
    let mut rules = snapshot(tx, graph).await?;
    for &start in from {
        let chain: Vec<Uuid> = std::iter::once(start)
            .chain(rules.ancestors(start))
            .collect();
        for id in chain {
            let Some((was, automatic)) = rules
                .items
                .get(&id)
                .map(|item| (item.state, item.automatic))
            else {
                continue;
            };
            let now = rules.settle(id);
            if now == was {
                // A state set by hand (or restored by an undo) that the rules would set anyway is theirs from now on, so
                // it follows its links and parts.
                if !automatic && rules.derived(id) {
                    sqlx::query("update work_items set state_by_rules = true where id = $1")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    if let Some(item) = rules.items.get_mut(&id) {
                        item.automatic = true;
                    }
                }
                continue;
            }
            set_state(tx, id, now, true).await?;
            record_as(
                tx,
                actor,
                graph,
                Some(id),
                "state",
                Some(json!({"state": was})),
                Some(json!({"state": now})),
                None,
                true,
            )
            .await?;
            if let Some(item) = rules.items.get_mut(&id) {
                (item.state, item.automatic) = (now, true);
            }
        }
    }
    Ok(())
}

/// The rank for an item going among `parent`'s parts (or the top level) right after `after` (`Some(None)`: first;
/// `None`: last). Spreads the siblings out first if there's no room left between the two.
async fn place(
    tx: &mut PgConnection,
    rules: &rules::Graph,
    parent: Option<Uuid>,
    moving: Option<Uuid>,
    after: Option<Option<Uuid>>,
) -> WorkResult<f64> {
    let mut siblings: Vec<(Uuid, f64)> = rules
        .items
        .values()
        .filter(|item| item.parent == parent && Some(item.id) != moving)
        .map(|item| (item.id, item.rank))
        .collect();
    siblings.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    let index = match after {
        None => siblings.len(),
        Some(None) => 0,
        Some(Some(sibling)) => match siblings.iter().position(|(id, _)| *id == sibling) {
            Some(at) => at + 1,
            None => return invalid("`after` names an item that isn't among its new siblings"),
        },
    };
    let around = |siblings: &[(Uuid, f64)]| {
        (
            index.checked_sub(1).map(|i| siblings[i].1),
            siblings.get(index).map(|s| s.1),
        )
    };
    let (before, next) = around(&siblings);
    if let Some(rank) = rules::rank_between(before, next) {
        return Ok(rank);
    }
    for (n, (id, rank)) in siblings.iter_mut().enumerate() {
        *rank = (n + 1) as f64;
        sqlx::query("update work_items set rank = $2 where id = $1")
            .bind(*id)
            .bind(*rank)
            .execute(&mut *tx)
            .await?;
    }
    let (before, next) = around(&siblings);
    Ok(rules::rank_between(before, next).unwrap_or(index as f64 + 0.5))
}

/// The items of a graph, or just `only`, each with its links and edges.
async fn items(db: &PgPool, graph: Uuid, only: Option<Uuid>) -> sqlx::Result<Vec<Item>> {
    let mut items: Vec<Item> = sqlx::query_as(
        "select id, graph_id, parent_id, title, description, state, rank, created_at, updated_at \
         from work_items where graph_id = $1 and ($2::uuid is null or id = $2) order by rank, id",
    )
    .bind(graph)
    .bind(only)
    .fetch_all(db)
    .await?;
    let links: Vec<Link> = sqlx::query_as(
        "select l.id, l.item_id, l.kind, l.target, l.role, l.status, l.created_at from work_links l \
         join work_items i on i.id = l.item_id where i.graph_id = $1 order by l.created_at, l.id",
    )
    .bind(graph)
    .fetch_all(db)
    .await?;
    let edges: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "select blocker_id, blocked_id from work_edges where graph_id = $1 order by created_at",
    )
    .bind(graph)
    .fetch_all(db)
    .await?;
    let mut by_id: HashMap<Uuid, &mut Item> = items.iter_mut().map(|i| (i.id, i)).collect();
    for link in links {
        if let Some(item) = by_id.get_mut(&link.item_id) {
            item.links.push(link);
        }
    }
    for (blocker, blocked) in edges {
        if let Some(item) = by_id.get_mut(&blocker) {
            item.blocks.push(blocked);
        }
        if let Some(item) = by_id.get_mut(&blocked) {
            item.blocked_by.push(blocker);
        }
    }
    Ok(items)
}

/// One of the owner's graphs with all its items.
pub async fn graph_items(db: &PgPool, owner: &str, graph: Uuid) -> sqlx::Result<Option<Vec<Item>>> {
    if self::graph(db, owner, graph).await?.is_none() {
        return Ok(None);
    }
    items(db, graph, None).await.map(Some)
}

pub async fn item(db: &PgPool, owner: &str, id: Uuid) -> sqlx::Result<Option<Item>> {
    let graph: Option<Uuid> = sqlx::query_scalar(
        "select i.graph_id from work_items i join work_graphs g on g.id = i.graph_id \
         where i.id = $1 and g.owner = $2",
    )
    .bind(id)
    .bind(owner)
    .fetch_optional(db)
    .await?;
    let Some(graph) = graph else {
        return Ok(None);
    };
    Ok(items(db, graph, Some(id)).await?.pop())
}

async fn item_after(db: &PgPool, owner: &str, id: Uuid) -> WorkResult<Item> {
    item(db, owner, id).await?.ok_or(WorkError::NotFound)
}

/// What's ready to start in one of the owner's graphs, in order (`rules::Graph::frontier`).
pub async fn frontier(db: &PgPool, owner: &str, graph: Uuid) -> sqlx::Result<Option<Vec<Item>>> {
    let Some(all) = graph_items(db, owner, graph).await? else {
        return Ok(None);
    };
    let mut conn = db.acquire().await?;
    let order = snapshot(&mut conn, graph).await?.frontier();
    let mut by_id: HashMap<Uuid, Item> = all.into_iter().map(|i| (i.id, i)).collect();
    Ok(Some(
        order.iter().filter_map(|id| by_id.remove(id)).collect(),
    ))
}

pub async fn create_item(
    db: &PgPool,
    actor: Actor<'_>,
    graph: Uuid,
    new: &NewItem,
) -> WorkResult<Item> {
    check_title(&new.title)?;
    check_description(&new.description)?;
    let mut tx = db.begin().await?;
    lock_graph(&mut tx, actor.user, graph).await?;
    let rules = snapshot(&mut tx, graph).await?;
    if let Some(parent) = new.parent_id
        && !rules.items.contains_key(&parent)
    {
        return invalid("parent_id isn't an item in this graph");
    }
    let rank = place(&mut tx, &rules, new.parent_id, None, new.after).await?;
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into work_items (id, graph_id, parent_id, title, description, rank) \
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(graph)
    .bind(new.parent_id)
    .bind(new.title.trim())
    .bind(new.description.trim())
    .bind(rank)
    .execute(&mut *tx)
    .await?;
    record(
        &mut tx,
        actor,
        graph,
        Some(id),
        "create",
        None,
        Some(json!({
            "title": new.title.trim(),
            "description": new.description.trim(),
            "parent_id": new.parent_id,
            "rank": rank,
        })),
        None,
    )
    .await?;
    settle(&mut tx, actor, graph, &[id]).await?;
    tx.commit().await?;
    item_after(db, actor.user, id).await
}

pub async fn edit_item(
    db: &PgPool,
    actor: Actor<'_>,
    id: Uuid,
    edit: &ItemEdit,
) -> WorkResult<Item> {
    if let Some(title) = &edit.title {
        check_title(title)?;
    }
    if let Some(description) = &edit.description {
        check_description(description)?;
    }
    let mut tx = db.begin().await?;
    let graph = lock_item_graph(&mut tx, actor.user, id).await?;
    let (title, description): (String, String) =
        sqlx::query_as("select title, description from work_items where id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let before = json!({"title": title, "description": description});
    let after = json!({
        "title": edit.title.as_deref().map(str::trim).unwrap_or(&title),
        "description": edit.description.as_deref().map(str::trim).unwrap_or(&description),
    });
    if before != after {
        write_text(&mut tx, id, &after).await?;
        record(
            &mut tx,
            actor,
            graph,
            Some(id),
            "edit",
            Some(before),
            Some(after),
            None,
        )
        .await?;
    }
    tx.commit().await?;
    item_after(db, actor.user, id).await
}

async fn write_text(tx: &mut PgConnection, id: Uuid, text: &Value) -> sqlx::Result<()> {
    sqlx::query(
        "update work_items set title = $2, description = $3, updated_at = now() where id = $1",
    )
    .bind(id)
    .bind(text["title"].as_str().unwrap_or_default())
    .bind(text["description"].as_str().unwrap_or_default())
    .execute(&mut *tx)
    .await
    .map(|_| ())
}

/// Moves an item to a state by hand, within the rules (`rules::Graph::refuse_move`).
pub async fn move_state(db: &PgPool, actor: Actor<'_>, id: Uuid, to: State) -> WorkResult<Item> {
    change_state(db, actor, id, to, true).await
}

/// Moves an item to a state as tracking does (ADR 30's `track`): within the rules, and never into or out of `dropped`,
/// which is planning. Checked under the graph's lock, so a drop that lands first isn't undone.
pub async fn track_state(db: &PgPool, actor: Actor<'_>, id: Uuid, to: State) -> WorkResult<Item> {
    change_state(db, actor, id, to, false).await
}

async fn change_state(
    db: &PgPool,
    actor: Actor<'_>,
    id: Uuid,
    to: State,
    plans: bool,
) -> WorkResult<Item> {
    let mut tx = db.begin().await?;
    let graph = lock_item_graph(&mut tx, actor.user, id).await?;
    let rules = snapshot(&mut tx, graph).await?;
    let was = rules.items[&id].state;
    if !plans && (was == State::Dropped || to == State::Dropped) {
        return conflict(
            "dropping an item, and reopening a dropped one, are plan changes: propose them with propose_work_plan",
        );
    }
    if let Some(why) = rules.refuse_move(id, to) {
        return conflict(why);
    }
    if was != to {
        set_state(&mut tx, id, to, false).await?;
        record(
            &mut tx,
            actor,
            graph,
            Some(id),
            "state",
            Some(json!({"state": was})),
            Some(json!({"state": to})),
            None,
        )
        .await?;
        settle(&mut tx, actor, graph, &[id]).await?;
    }
    tx.commit().await?;
    item_after(db, actor.user, id).await
}

/// Reparents and reranks an item.
pub async fn move_item(db: &PgPool, actor: Actor<'_>, id: Uuid, to: &ItemMove) -> WorkResult<Item> {
    let mut tx = db.begin().await?;
    let graph = lock_item_graph(&mut tx, actor.user, id).await?;
    let rules = snapshot(&mut tx, graph).await?;
    let item = rules.items[&id].clone();
    let parent = to.parent_id.unwrap_or(item.parent);
    if let Some(parent) = parent
        && Some(parent) != item.parent
    {
        if !rules.items.contains_key(&parent) {
            return invalid("parent_id isn't an item in this graph");
        }
        if let Some(why) = rules.refuse_parent(id, parent) {
            return conflict(why);
        }
    }
    let rank = place(&mut tx, &rules, parent, Some(id), to.after).await?;
    reposition(&mut tx, id, parent, rank).await?;
    record(
        &mut tx,
        actor,
        graph,
        Some(id),
        "move",
        Some(json!({"parent_id": item.parent, "rank": item.rank})),
        Some(json!({"parent_id": parent, "rank": rank})),
        None,
    )
    .await?;
    let mut touched = vec![id];
    touched.extend(item.parent);
    settle(&mut tx, actor, graph, &touched).await?;
    tx.commit().await?;
    item_after(db, actor.user, id).await
}

async fn reposition(
    tx: &mut PgConnection,
    id: Uuid,
    parent: Option<Uuid>,
    rank: f64,
) -> sqlx::Result<()> {
    sqlx::query("update work_items set parent_id = $2, rank = $3, updated_at = now() where id = $1")
        .bind(id)
        .bind(parent)
        .bind(rank)
        .execute(&mut *tx)
        .await
        .map(|_| ())
}

/// Makes `blocker` block `blocked`; both in the same graph, and never closing a cycle.
pub async fn add_block(
    db: &PgPool,
    actor: Actor<'_>,
    blocker: Uuid,
    blocked: Uuid,
) -> WorkResult<Item> {
    let mut tx = db.begin().await?;
    let graph = lock_item_graph(&mut tx, actor.user, blocker).await?;
    let rules = snapshot(&mut tx, graph).await?;
    if !rules.items.contains_key(&blocked) {
        return invalid("the item to block isn't in this graph");
    }
    if !rules.blocks.contains(&(blocker, blocked)) {
        if let Some(why) = rules.refuse_block(blocker, blocked) {
            return conflict(why);
        }
        insert_block(&mut tx, graph, blocker, blocked).await?;
        let edge = json!({"blocker": blocker, "blocked": blocked});
        record(
            &mut tx,
            actor,
            graph,
            Some(blocked),
            "block",
            None,
            Some(edge),
            None,
        )
        .await?;
    }
    tx.commit().await?;
    item_after(db, actor.user, blocker).await
}

async fn insert_block(
    tx: &mut PgConnection,
    graph: Uuid,
    blocker: Uuid,
    blocked: Uuid,
) -> sqlx::Result<()> {
    sqlx::query("insert into work_edges (graph_id, blocker_id, blocked_id) values ($1, $2, $3)")
        .bind(graph)
        .bind(blocker)
        .bind(blocked)
        .execute(&mut *tx)
        .await
        .map(|_| ())
}

async fn delete_block(tx: &mut PgConnection, blocker: Uuid, blocked: Uuid) -> sqlx::Result<bool> {
    Ok(
        sqlx::query("delete from work_edges where blocker_id = $1 and blocked_id = $2")
            .bind(blocker)
            .bind(blocked)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn remove_block(
    db: &PgPool,
    actor: Actor<'_>,
    blocker: Uuid,
    blocked: Uuid,
) -> WorkResult<Item> {
    let mut tx = db.begin().await?;
    let graph = lock_item_graph(&mut tx, actor.user, blocker).await?;
    if !delete_block(&mut tx, blocker, blocked).await? {
        return Err(WorkError::NotFound);
    }
    let edge = json!({"blocker": blocker, "blocked": blocked});
    record(
        &mut tx,
        actor,
        graph,
        Some(blocked),
        "unblock",
        Some(edge),
        None,
        None,
    )
    .await?;
    tx.commit().await?;
    item_after(db, actor.user, blocker).await
}

const LINK_COLUMNS: &str = "id, item_id, kind, target, role, status, created_at";

pub async fn add_link(
    db: &PgPool,
    actor: Actor<'_>,
    item: Uuid,
    new: &NewLink,
) -> WorkResult<Link> {
    let target = new.target.trim();
    let mut tx = db.begin().await?;
    let graph = lock_item_graph(&mut tx, actor.user, item).await?;
    let target = check_target(&mut tx, actor.user, new.kind, target).await?;
    let link: Option<Link> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "insert into work_links (id, item_id, kind, target, role) values ($1, $2, $3, $4, $5) \
         on conflict (item_id, kind, target) do nothing returning {LINK_COLUMNS}"
    )))
    .bind(Uuid::now_v7())
    .bind(item)
    .bind(kind_str(new.kind))
    .bind(&target)
    .bind(role_str(new.role))
    .fetch_optional(&mut *tx)
    .await?;
    let Some(link) = link else {
        return conflict("the item links that already");
    };
    record(
        &mut tx,
        actor,
        graph,
        Some(item),
        "link",
        None,
        Some(json!(link)),
        None,
    )
    .await?;
    settle(&mut tx, actor, graph, &[item]).await?;
    tx.commit().await?;
    Ok(link)
}

/// Locks the graph one of the owner's links is in, and returns it with the link.
async fn lock_link(tx: &mut PgConnection, owner: &str, id: Uuid) -> WorkResult<(Uuid, Link)> {
    let graph: Option<Uuid> = sqlx::query_scalar(
        "select g.id from work_links l join work_items i on i.id = l.item_id \
         join work_graphs g on g.id = i.graph_id where l.id = $1 and g.owner = $2 for update of g",
    )
    .bind(id)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    let graph = graph.ok_or(WorkError::NotFound)?;
    let link: Link = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {LINK_COLUMNS} from work_links where id = $1"
    )))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    Ok((graph, link))
}

/// The item one of the owner's links is on.
pub async fn link_item(db: &PgPool, owner: &str, link: Uuid) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar(
        "select l.item_id from work_links l join work_items i on i.id = l.item_id \
         join work_graphs g on g.id = i.graph_id where l.id = $1 and g.owner = $2",
    )
    .bind(link)
    .bind(owner)
    .fetch_optional(db)
    .await
}

/// Records where what a link points at stands. Later stages do this as they watch pull requests, sessions and
/// proposals; for now the owner does.
pub async fn set_link_status(
    db: &PgPool,
    actor: Actor<'_>,
    id: Uuid,
    status: LinkStatus,
) -> WorkResult<Link> {
    let mut tx = db.begin().await?;
    let (graph, link) = lock_link(&mut tx, actor.user, id).await?;
    if link.status != status_str(status) {
        write_link_status(&mut tx, id, status_str(status)).await?;
        record(
            &mut tx,
            actor,
            graph,
            Some(link.item_id),
            "link_status",
            Some(json!({"link": id, "status": link.status})),
            Some(json!({"link": id, "status": status})),
            None,
        )
        .await?;
        settle(&mut tx, actor, graph, &[link.item_id]).await?;
    }
    let link: Link = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {LINK_COLUMNS} from work_links where id = $1"
    )))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(link)
}

async fn write_link_status(tx: &mut PgConnection, id: Uuid, status: &str) -> sqlx::Result<()> {
    sqlx::query("update work_links set status = $2 where id = $1")
        .bind(id)
        .bind(status)
        .execute(&mut *tx)
        .await
        .map(|_| ())
}

pub async fn remove_link(db: &PgPool, actor: Actor<'_>, id: Uuid) -> WorkResult<()> {
    let mut tx = db.begin().await?;
    let (graph, link) = lock_link(&mut tx, actor.user, id).await?;
    sqlx::query("delete from work_links where id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    record(
        &mut tx,
        actor,
        graph,
        Some(link.item_id),
        "unlink",
        Some(json!(link)),
        None,
        None,
    )
    .await?;
    settle(&mut tx, actor, graph, &[link.item_id]).await?;
    tx.commit().await?;
    Ok(())
}

/// An item's history, oldest first: its own changes, and the edges it blocks through.
pub async fn item_history(db: &PgPool, owner: &str, id: Uuid) -> sqlx::Result<Option<Vec<Event>>> {
    if item(db, owner, id).await?.is_none() {
        return Ok(None);
    }
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {EVENT_COLUMNS} from work_events where item_id = $1 \
         or (kind in ('block', 'unblock') and coalesce(after, before)->>'blocker' = $1::text) \
         order by at, id"
    )))
    .bind(id)
    .fetch_all(db)
    .await
    .map(Some)
}

/// A graph's history, oldest first.
pub async fn graph_history(db: &PgPool, owner: &str, id: Uuid) -> sqlx::Result<Option<Vec<Event>>> {
    if graph(db, owner, id).await?.is_none() {
        return Ok(None);
    }
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {EVENT_COLUMNS} from work_events where graph_id = $1 order by at, id"
    )))
    .bind(id)
    .fetch_all(db)
    .await
    .map(Some)
}

fn uuid_at(value: &Option<Value>, key: &str) -> Option<Uuid> {
    value.as_ref()?.get(key)?.as_str()?.parse().ok()
}

/// Undoes a change by making its inverse, recorded as a change of its own that `undoes` it. Only a change whose
/// effect is still there can be undone: undoing a rename after a later one would silently drop the later one.
/// Undoing an item's creation drops it, keeping its history. The rules' own state changes aren't undone: they follow
/// links and parts, which are what to change.
pub async fn undo(db: &PgPool, actor: Actor<'_>, id: Uuid) -> WorkResult<Event> {
    let mut tx = db.begin().await?;
    let graph: Option<Uuid> = sqlx::query_scalar(
        "select g.id from work_events e join work_graphs g on g.id = e.graph_id \
         where e.id = $1 and g.owner = $2 for update of g",
    )
    .bind(id)
    .bind(actor.user)
    .fetch_optional(&mut *tx)
    .await?;
    let graph = graph.ok_or(WorkError::NotFound)?;
    let event: Event = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select {EVENT_COLUMNS} from work_events where id = $1"
    )))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if event.automatic {
        return conflict(
            "the rules made that change, from the item's links and parts; change those instead",
        );
    }
    let undone: bool =
        sqlx::query_scalar("select exists (select 1 from work_events where undoes = $1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if undone {
        return conflict("that change has been undone already");
    }
    let changed_since = || conflict("it has changed since; undo the later changes first");
    let rules = snapshot(&mut tx, graph).await?;
    let item = event.item_id;
    let current = item.and_then(|i| rules.items.get(&i));
    let mut kind = event.kind.as_str();
    let (mut before, mut after) = (event.after.clone(), event.before.clone());
    let mut touched = Vec::new();
    match kind {
        "rename" => {
            let name: String = sqlx::query_scalar("select name from work_graphs where id = $1")
                .bind(graph)
                .fetch_one(&mut *tx)
                .await?;
            if before.as_ref().map(|b| &b["name"]) != Some(&json!(name)) {
                return changed_since();
            }
            let to = after
                .as_ref()
                .and_then(|a| a["name"].as_str())
                .unwrap_or_default()
                .to_owned();
            set_graph_name(&mut tx, graph, &to).await?;
        }
        "create" => {
            let Some(current) = current else {
                return Err(WorkError::NotFound);
            };
            if current.state == State::Dropped {
                return conflict("the item is dropped already");
            }
            kind = "state";
            before = Some(json!({"state": current.state}));
            after = Some(json!({"state": State::Dropped}));
            set_state(&mut tx, current.id, State::Dropped, false).await?;
            touched.push(current.id);
        }
        "edit" => {
            let Some(current) = current else {
                return Err(WorkError::NotFound);
            };
            let (title, description): (String, String) =
                sqlx::query_as("select title, description from work_items where id = $1")
                    .bind(current.id)
                    .fetch_one(&mut *tx)
                    .await?;
            if before != Some(json!({"title": title, "description": description})) {
                return changed_since();
            }
            write_text(&mut tx, current.id, after.as_ref().unwrap_or(&Value::Null)).await?;
        }
        "state" => {
            let Some(current) = current else {
                return Err(WorkError::NotFound);
            };
            if before != Some(json!({"state": current.state})) {
                return changed_since();
            }
            let to = after
                .as_ref()
                .and_then(|a| a["state"].as_str())
                .and_then(State::parse)
                .unwrap_or(State::Open);
            set_state(&mut tx, current.id, to, false).await?;
            touched.push(current.id);
        }
        "move" => {
            let Some(current) = current else {
                return Err(WorkError::NotFound);
            };
            if before != Some(json!({"parent_id": current.parent, "rank": current.rank})) {
                return changed_since();
            }
            let parent = uuid_at(&after, "parent_id");
            if let Some(parent) = parent {
                if !rules.items.contains_key(&parent) {
                    return Err(WorkError::NotFound);
                }
                if let Some(why) = rules.refuse_parent(current.id, parent) {
                    return conflict(why);
                }
            }
            let rank = after
                .as_ref()
                .and_then(|a| a["rank"].as_f64())
                .unwrap_or(current.rank);
            reposition(&mut tx, current.id, parent, rank).await?;
            touched.push(current.id);
            touched.extend(current.parent);
        }
        "block" => {
            let (Some(blocker), Some(blocked)) =
                (uuid_at(&before, "blocker"), uuid_at(&before, "blocked"))
            else {
                return Err(WorkError::NotFound);
            };
            if !delete_block(&mut tx, blocker, blocked).await? {
                return changed_since();
            }
            kind = "unblock";
        }
        "unblock" => {
            let (Some(blocker), Some(blocked)) =
                (uuid_at(&after, "blocker"), uuid_at(&after, "blocked"))
            else {
                return Err(WorkError::NotFound);
            };
            if !rules.items.contains_key(&blocker) || !rules.items.contains_key(&blocked) {
                return Err(WorkError::NotFound);
            }
            if rules.blocks.contains(&(blocker, blocked)) {
                return changed_since();
            }
            if let Some(why) = rules.refuse_block(blocker, blocked) {
                return conflict(why);
            }
            insert_block(&mut tx, graph, blocker, blocked).await?;
            kind = "block";
        }
        "link" => {
            let Some(link) = uuid_at(&before, "id") else {
                return Err(WorkError::NotFound);
            };
            // Only the link as it was made: once its status has changed, undoing that comes first.
            let current: Option<Link> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "select {LINK_COLUMNS} from work_links where id = $1"
            )))
            .bind(link)
            .fetch_optional(&mut *tx)
            .await?;
            if current.map(|l| json!(l)) != before {
                return changed_since();
            }
            sqlx::query("delete from work_links where id = $1")
                .bind(link)
                .execute(&mut *tx)
                .await?;
            kind = "unlink";
            touched.extend(item);
        }
        "unlink" => {
            let Some(link) = after
                .clone()
                .and_then(|a| serde_json::from_value::<Link>(a).ok())
            else {
                return Err(WorkError::NotFound);
            };
            let restored = sqlx::query(
                "insert into work_links (id, item_id, kind, target, role, status, created_at) \
                 values ($1, $2, $3, $4, $5, $6, $7) on conflict do nothing",
            )
            .bind(link.id)
            .bind(link.item_id)
            .bind(&link.kind)
            .bind(&link.target)
            .bind(&link.role)
            .bind(&link.status)
            .bind(link.created_at)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if restored == 0 {
                return conflict("the item links that again already");
            }
            kind = "link";
            touched.push(link.item_id);
        }
        "link_status" => {
            let Some(link) = uuid_at(&before, "link") else {
                return Err(WorkError::NotFound);
            };
            let status: Option<String> =
                sqlx::query_scalar("select status from work_links where id = $1")
                    .bind(link)
                    .fetch_optional(&mut *tx)
                    .await?;
            if status.is_none() || before.as_ref().map(|b| &b["status"]) != Some(&json!(status)) {
                return changed_since();
            }
            let to = after
                .as_ref()
                .and_then(|a| a["status"].as_str())
                .unwrap_or("open")
                .to_owned();
            write_link_status(&mut tx, link, &to).await?;
            touched.extend(item);
        }
        _ => return conflict(format!("a {} change can't be undone", event.kind)),
    }
    let inverse = record(&mut tx, actor, graph, item, kind, before, after, Some(id)).await?;
    settle(&mut tx, actor, graph, &touched).await?;
    tx.commit().await?;
    Ok(inverse)
}

pub mod follow;

#[cfg(test)]
mod tests;
