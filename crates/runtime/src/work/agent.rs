//! Agents on the work graph (ADR 30): the `work` grant and its levels, sloppod's MCP tools for reading and tracking a
//! graph, `plan` changes as proposals, and what a session is told about the graph and the items it's linked to.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sloppod_contracts::{
    Error, Grant, Manifest, ProposalKind, Result, SessionContext, ToolSpec, capability,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::rules::{LinkKind, LinkStatus, Role, State};
use super::{Actor, ItemEdit, ItemMove, NewItem, NewLink, WorkError};

/// What a `work` grant lets a session do with its graph (ADR 30), each level including the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// See the graph.
    Read,
    /// Also add items and edges, link sessions, pull requests and issues, and move items between open, active and
    /// done.
    Track,
    /// Also rank, reparent, drop and reopen items: as proposals, when an agent does it.
    Plan,
}

impl Level {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "read" => Level::Read,
            "track" => Level::Track,
            "plan" => Level::Plan,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Level::Read => "read",
            Level::Track => "track",
            Level::Plan => "plan",
        }
    }
}

/// The graphs a session's `work` grants cover, each with its level.
pub fn granted(session: &SessionContext) -> Vec<(Uuid, Level)> {
    session
        .grants
        .iter()
        .filter(|g| g.capability == capability::WORK)
        .filter_map(|g| {
            Some((
                g.param("graph")?.parse().ok()?,
                Level::parse(g.param("level")?)?,
            ))
        })
        .collect()
}

/// A grant of one of the owner's graphs at `level`, for `POST /sessions/{id}/grants`.
pub async fn grant(db: &PgPool, owner: &str, params: &Value) -> std::result::Result<Grant, String> {
    let graph: Uuid = params["graph"]
        .as_str()
        .and_then(|g| g.parse().ok())
        .ok_or("a work grant names its `graph` by id")?;
    let level = match params["level"].as_str() {
        None => Level::Track,
        Some(level) => Level::parse(level).ok_or("`level` is read, track or plan")?,
    };
    let graph = super::graph(db, owner, graph)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("that isn't one of your work graphs")?;
    Ok(Grant::new(
        capability::WORK,
        json!({"graph": graph.id, "name": graph.name, "level": level.as_str()}),
    ))
}

/// A sub-session's grant of its parent's graph: at most `track` (ADR 30's attenuation), and no more than the parent's.
pub fn inherited(grant: &mut Grant) {
    let level = grant
        .param("level")
        .and_then(Level::parse)
        .unwrap_or(Level::Read)
        .min(Level::Track);
    grant.params["level"] = json!(level.as_str());
}

/// The graph a call is about: the one it names, which the session must hold a grant of, or else its only one.
fn graph_for(
    session: &SessionContext,
    arguments: &Value,
) -> std::result::Result<(Uuid, Level), String> {
    let granted = granted(session);
    match arguments["graph"].as_str() {
        Some(named) => {
            let id: Uuid = named.parse().map_err(|_| "`graph` is a graph's id")?;
            granted
                .into_iter()
                .find(|(graph, _)| *graph == id)
                .ok_or_else(|| "this session has no work grant of that graph".into())
        }
        None => match granted.as_slice() {
            [] => Err("this session has no work grant".into()),
            [only] => Ok(*only),
            _ => Err("this session has several work graphs; say which with `graph`".into()),
        },
    }
}

fn uuid_arg(arguments: &Value, key: &str) -> std::result::Result<Uuid, String> {
    arguments[key]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("give `{key}`, an id"))
}

fn work_error(e: WorkError) -> String {
    match e {
        WorkError::NotFound => "no such item or link in this graph".into(),
        WorkError::Invalid(message) | WorkError::Conflict(message) => message,
        WorkError::Db(e) => e.to_string(),
    }
}

/// Checks that `item` is in `graph`, so a grant of one graph reaches nothing in another.
async fn in_graph(
    db: &PgPool,
    owner: &str,
    graph: Uuid,
    item: Uuid,
) -> std::result::Result<super::Item, String> {
    match super::item(db, owner, item)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(found) if found.graph_id == graph => Ok(found),
        _ => Err("no such item in this graph".into()),
    }
}

/// An item as an agent reads it: what it needs to plan, without the timestamps.
fn brief(item: &super::Item, with_description: bool) -> Value {
    let mut brief = json!({
        "id": item.id,
        "title": item.title,
        "state": item.state,
        "parent_id": item.parent_id,
        "blocked_by": item.blocked_by,
        "links": item.links.iter().map(|l| json!({
            "id": l.id, "kind": l.kind, "target": l.target, "role": l.role, "status": l.status,
        })).collect::<Vec<_>>(),
    });
    if with_description {
        brief["description"] = json!(item.description);
    }
    brief
}

pub const LIST: &str = "list_work";
pub const UPDATE: &str = "update_work";
pub const PROPOSE_PLAN: &str = "propose_work_plan";

/// The work tools a session gets: none without a `work` grant, `update_work` from `track`, and the plan proposal's
/// tool only at `plan`.
pub fn shown(session: &SessionContext, tool: &str) -> bool {
    let best = granted(session).into_iter().map(|(_, level)| level).max();
    match tool {
        LIST => best.is_some(),
        UPDATE => best >= Some(Level::Track),
        PROPOSE_PLAN => best == Some(Level::Plan),
        _ => true,
    }
}

pub fn tools(session: &SessionContext) -> Vec<Value> {
    let tools = [
        json!({
            "name": LIST,
            "description": "Read the work graph this session is granted (ADR 30): its items, what blocks what and \
                what each links to. `view: \"frontier\"` lists what's ready to start, in order; read it after your \
                context was compacted rather than relying on memory. `view: \"item\"` shows one item with its \
                description and history.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "graph": {"type": "string", "description": "The graph's id; defaults to the only one granted"},
                    "view": {"type": "string", "enum": ["graph", "frontier", "item"], "default": "graph"},
                    "item": {"type": "string", "description": "The item's id, for `view: \"item\"`"},
                },
            },
        }),
        json!({
            "name": UPDATE,
            "description": "Track work in the graph, applied at once and recorded in the item's history, which the \
                user can undo: add an item (`add_item`, last among its siblings), edit one (`edit_item`), move it \
                between open, active and done (`set_state`), say one blocks another (`block`, `unblock`), and link \
                an issue, pull request, session or proposal (`link`, with `role` completes or related; a session \
                link defaults to this session) or change or remove a link (`link_status`, `unlink`). An item is \
                done by itself once its completing links finish and its parts are done. Ranking, reparenting, \
                dropping and reopening are plan changes: propose_work_plan.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "graph": {"type": "string", "description": "The graph's id; defaults to the only one granted"},
                    "action": {"type": "string", "enum": ["add_item", "edit_item", "set_state", "block", "unblock", "link", "unlink", "link_status"]},
                    "item": {"type": "string", "description": "The item's id (every action but add_item, unlink and link_status)"},
                    "title": {"type": "string"},
                    "description": {"type": "string"},
                    "parent_id": {"type": "string", "description": "For add_item: the item it's part of"},
                    "state": {"type": "string", "enum": ["open", "active", "done"]},
                    "blocks": {"type": "string", "description": "For block and unblock: the item that `item` blocks"},
                    "kind": {"type": "string", "enum": ["issue", "pull_request", "session", "proposal"]},
                    "target": {"type": "string", "description": "owner/repo#n for an issue or pull request, an id for a session or proposal"},
                    "role": {"type": "string", "enum": ["completes", "related"]},
                    "link": {"type": "string", "description": "The link's id, for unlink and link_status"},
                    "status": {"type": "string", "enum": ["open", "finished", "abandoned"]},
                },
                "required": ["action"],
            },
        }),
    ];
    tools
        .into_iter()
        .filter(|tool| shown(session, tool["name"].as_str().unwrap_or_default()))
        .collect()
}

/// Runs `list_work` or `update_work` for a session.
pub async fn call(
    db: &PgPool,
    session: &SessionContext,
    name: &str,
    arguments: &Value,
) -> std::result::Result<String, String> {
    let (graph, level) = graph_for(session, arguments)?;
    let owner = session.owner.as_str();
    if name == LIST {
        return list(db, owner, graph, arguments).await;
    }
    if level < Level::Track {
        return Err("this session's work grant of that graph is read only".into());
    }
    let actor = Actor {
        user: owner,
        session: session.session_id.parse().ok(),
    };
    let item = || uuid_arg(arguments, "item");
    let text = |key: &str| arguments[key].as_str().map(str::to_owned);
    let done = |item: super::Item| Ok(brief(&item, false).to_string());
    match arguments["action"].as_str().unwrap_or_default() {
        "add_item" => {
            let parent_id = match arguments["parent_id"].as_str() {
                Some(_) => Some(
                    in_graph(db, owner, graph, uuid_arg(arguments, "parent_id")?)
                        .await?
                        .id,
                ),
                None => None,
            };
            let new = NewItem {
                title: text("title").ok_or("give the item a `title`")?,
                description: text("description").unwrap_or_default(),
                parent_id,
                after: None,
            };
            done(
                super::create_item(db, actor, graph, &new)
                    .await
                    .map_err(work_error)?,
            )
        }
        "edit_item" => {
            let item = in_graph(db, owner, graph, item()?).await?;
            let edit = ItemEdit {
                title: text("title"),
                description: text("description"),
            };
            done(
                super::edit_item(db, actor, item.id, &edit)
                    .await
                    .map_err(work_error)?,
            )
        }
        "set_state" => {
            let item = in_graph(db, owner, graph, item()?).await?;
            let to = arguments["state"]
                .as_str()
                .and_then(State::parse)
                .ok_or("`state` is open, active or done")?;
            if to == State::Dropped || item.state == State::Dropped.as_str() {
                return Err(plan_change(level));
            }
            done(
                super::move_state(db, actor, item.id, to)
                    .await
                    .map_err(work_error)?,
            )
        }
        action @ ("block" | "unblock") => {
            let blocker = in_graph(db, owner, graph, item()?).await?;
            let blocked = in_graph(db, owner, graph, uuid_arg(arguments, "blocks")?).await?;
            let changed = if action == "block" {
                super::add_block(db, actor, blocker.id, blocked.id).await
            } else {
                super::remove_block(db, actor, blocker.id, blocked.id).await
            };
            done(changed.map_err(work_error)?)
        }
        "link" => {
            let item = in_graph(db, owner, graph, item()?).await?;
            let kind: LinkKind = serde_json::from_value(arguments["kind"].clone())
                .map_err(|_| "`kind` is issue, pull_request, session or proposal")?;
            let target = match (text("target"), kind) {
                (Some(target), _) => target,
                (None, LinkKind::Session) => session.session_id.clone(),
                (None, _) => return Err("give the link's `target`".into()),
            };
            let role: Role = serde_json::from_value(arguments["role"].clone())
                .map_err(|_| "`role` is completes or related")?;
            let link = super::add_link(db, actor, item.id, &NewLink { kind, target, role })
                .await
                .map_err(work_error)?;
            Ok(json!(link).to_string())
        }
        action @ ("unlink" | "link_status") => {
            let link = uuid_arg(arguments, "link")?;
            let on = super::link_item(db, owner, link)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("no such link in this graph")?;
            in_graph(db, owner, graph, on).await?;
            if action == "unlink" {
                super::remove_link(db, actor, link)
                    .await
                    .map_err(work_error)?;
                return Ok("Unlinked.".into());
            }
            let status: LinkStatus = serde_json::from_value(arguments["status"].clone())
                .map_err(|_| "`status` is open, finished or abandoned")?;
            let link = super::set_link_status(db, actor, link, status)
                .await
                .map_err(work_error)?;
            Ok(json!(link).to_string())
        }
        other => Err(format!("{other:?} isn't one of update_work's actions")),
    }
}

/// Why a plan change was refused, and what to do instead.
fn plan_change(level: Level) -> String {
    if level == Level::Plan {
        "dropping an item, and reopening a dropped one, are plan changes: propose them with propose_work_plan".into()
    } else {
        "dropping an item, and reopening a dropped one, are plan changes, which this session's work grant doesn't \
         cover; say what you'd change, and the user or your parent session decides"
            .into()
    }
}

async fn list(
    db: &PgPool,
    owner: &str,
    graph: Uuid,
    arguments: &Value,
) -> std::result::Result<String, String> {
    let found = |items: Option<Vec<super::Item>>| items.ok_or("that graph is gone".to_owned());
    match arguments["view"].as_str().unwrap_or("graph") {
        "frontier" => {
            let ready = found(
                super::frontier(db, owner, graph)
                    .await
                    .map_err(|e| e.to_string())?,
            )?;
            Ok(json!(ready.iter().map(|i| brief(i, false)).collect::<Vec<_>>()).to_string())
        }
        "item" => {
            let item = in_graph(db, owner, graph, uuid_arg(arguments, "item")?).await?;
            let history = super::item_history(db, owner, item.id)
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            let mut view = brief(&item, true);
            view["history"] = json!(history.iter().map(|e| json!({
                "kind": e.kind, "actor": e.actor, "session_id": e.session_id, "automatic": e.automatic,
                "before": e.before, "after": e.after, "at": e.at,
            })).collect::<Vec<_>>());
            Ok(view.to_string())
        }
        "graph" => {
            let items = found(
                super::graph_items(db, owner, graph)
                    .await
                    .map_err(|e| e.to_string())?,
            )?;
            Ok(json!(items.iter().map(|i| brief(i, false)).collect::<Vec<_>>()).to_string())
        }
        other => Err(format!("{other:?} isn't a view: graph, frontier or item")),
    }
}

/// What a session is told about its work: the graphs it's granted, the items it's linked to, and that the plan is
/// one call away. `None` when it has neither.
pub async fn prompt(db: &PgPool, session: &SessionContext) -> Option<String> {
    let linked: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "select i.id, i.title, i.state, g.name from work_links l join work_items i on i.id = l.item_id \
         join work_graphs g on g.id = i.graph_id \
         where l.kind = 'session' and l.target = $1 and g.owner = $2 order by g.name, i.rank",
    )
    .bind(&session.session_id)
    .bind(&session.owner)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    let grants: Vec<String> = session
        .grants
        .iter()
        .filter(|g| g.capability == capability::WORK)
        .map(|g| {
            format!(
                "`{}` ({}, at the `{}` level)",
                g.param("name").unwrap_or("?"),
                g.param("graph").unwrap_or("?"),
                g.param("level").unwrap_or("?")
            )
        })
        .collect();
    if linked.is_empty() && grants.is_empty() {
        return None;
    }
    let mut text = String::from("\n# Work items\n");
    if !grants.is_empty() {
        text.push_str(&format!(
            "The plan lives in sloppod's work graph, not in your context: this session can use {}. `list_work` \
             reads it; after a compaction, read the frontier (`view: \"frontier\"`) for what's ready next rather \
             than going by memory.\n",
            grants.join(", ")
        ));
    }
    if !linked.is_empty() {
        text.push_str("This session is linked to these items:\n");
        for (id, title, state, graph) in linked {
            text.push_str(&format!("- {title} ({state}, in `{graph}`, {id})\n"));
        }
    }
    Some(text)
}

/// A session's `plan` changes to its work graph (ADR 30): ranking, reparenting, dropping and reopening items. The
/// owner approves them, or a standing approval for the session does.
pub struct WorkPlan {
    db: PgPool,
}

impl WorkPlan {
    pub fn new(db: PgPool) -> Self {
        Self { db }
    }
}

pub const PLAN_KIND: &str = "sloppod.work.plan";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Change {
    Move,
    Drop,
    Reopen,
}

#[async_trait]
impl ProposalKind for WorkPlan {
    fn manifest(&self) -> Manifest {
        Manifest::builtin("work")
    }

    fn kind(&self) -> String {
        PLAN_KIND.into()
    }

    fn target(&self, payload: &Value) -> Option<String> {
        payload["graph"].as_str().map(str::to_owned)
    }

    fn summary(&self, payload: &Value) -> String {
        let changes = payload["changes"].as_array().map_or(0, Vec::len);
        format!(
            "{changes} change{} to the work graph {}",
            if changes == 1 { "" } else { "s" },
            payload["graph_name"].as_str().unwrap_or_default()
        )
    }

    fn tool(&self) -> ToolSpec {
        ToolSpec {
            name: PROPOSE_PLAN.into(),
            description: "Propose plan changes to the work graph: move an item (`move`: under another item with \
                          `parent_id`, null for the top level, and after a sibling with `after`, null for first), \
                          drop one (`drop`), or reopen a dropped one (`reopen`). The user approves them, or a \
                          standing approval for this session does; you're told when it's decided."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "graph": {"type": "string", "description": "The graph's id; defaults to the only one granted"},
                    "changes": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "item": {"type": "string"},
                                "action": {"type": "string", "enum": ["move", "drop", "reopen"]},
                                "parent_id": {"type": ["string", "null"]},
                                "after": {"type": ["string", "null"]},
                            },
                            "required": ["item", "action"],
                        },
                    },
                    "reason": {"type": "string", "description": "Why, shown to the user"},
                },
                "required": ["changes", "reason"],
            }),
        }
    }

    async fn prepare(&self, session: &SessionContext, arguments: Value) -> Result<Value> {
        let (graph, level) = graph_for(session, &arguments).map_err(Error::new)?;
        if level < Level::Plan {
            return Err(Error::new(plan_change(level)));
        }
        let name = super::graph(&self.db, &session.owner, graph)
            .await
            .map_err(|e| Error::new(e.to_string()))?
            .ok_or_else(|| Error::new("that graph is gone"))?
            .name;
        let asked = arguments["changes"].as_array().cloned().unwrap_or_default();
        if asked.is_empty() {
            return Err(Error::new("give at least one change"));
        }
        let mut changes = Vec::new();
        for change in asked {
            let item = uuid_arg(&change, "item").map_err(Error::new)?;
            let item = in_graph(&self.db, &session.owner, graph, item)
                .await
                .map_err(Error::new)?;
            let action: Change = serde_json::from_value(change["action"].clone())
                .map_err(|_| Error::new("a change's `action` is move, drop or reopen"))?;
            let mut prepared =
                json!({"item": item.id, "title": item.title, "action": change["action"]});
            if action == Change::Move {
                for key in ["parent_id", "after"] {
                    if let Some(value) = change.get(key) {
                        if let Some(id) = value.as_str() {
                            let id: Uuid = id
                                .parse()
                                .map_err(|_| Error::new(format!("`{key}` is an item's id")))?;
                            in_graph(&self.db, &session.owner, graph, id)
                                .await
                                .map_err(Error::new)?;
                        }
                        prepared[key] = value.clone();
                    }
                }
            }
            changes.push(prepared);
        }
        Ok(json!({
            "graph": graph,
            "graph_name": name,
            "changes": changes,
            "reason": arguments["reason"].as_str().unwrap_or_default(),
        }))
    }

    /// The items the changes touch, as they are now, so a reviewer sees what each change moves from.
    async fn detail(&self, session: &SessionContext, payload: &Value) -> Result<Value> {
        let mut current = Vec::new();
        for change in payload["changes"].as_array().into_iter().flatten() {
            let Some(id) = change["item"].as_str().and_then(|i| i.parse().ok()) else {
                continue;
            };
            if let Ok(Some(item)) = super::item(&self.db, &session.owner, id).await {
                current.push(json!({
                    "id": item.id, "title": item.title, "state": item.state, "parent_id": item.parent_id,
                    "rank": item.rank,
                }));
            }
        }
        Ok(json!({"current": current}))
    }

    async fn execute(
        &self,
        session: &SessionContext,
        _proposal: Uuid,
        payload: &Value,
    ) -> Result<Value> {
        let graph: Uuid = payload["graph"]
            .as_str()
            .and_then(|g| g.parse().ok())
            .ok_or_else(|| Error::new("the proposal names no graph"))?;
        let actor = Actor {
            user: &session.owner,
            session: session.session_id.parse().ok(),
        };
        let mut done = Vec::new();
        for change in payload["changes"].as_array().into_iter().flatten() {
            let title = change["title"].as_str().unwrap_or_default();
            let applied = async {
                let item: Uuid = uuid_arg(change, "item")?;
                in_graph(&self.db, &session.owner, graph, item).await?;
                let action: Change =
                    serde_json::from_value(change["action"].clone()).map_err(|e| e.to_string())?;
                let result = match action {
                    Change::Move => {
                        let to: ItemMove =
                            serde_json::from_value(change.clone()).map_err(|e| e.to_string())?;
                        super::move_item(&self.db, actor, item, &to).await
                    }
                    Change::Drop => super::move_state(&self.db, actor, item, State::Dropped).await,
                    Change::Reopen => super::move_state(&self.db, actor, item, State::Open).await,
                };
                result.map(|_| ()).map_err(work_error)
            }
            .await;
            if let Err(e) = applied {
                let before = if done.is_empty() {
                    String::new()
                } else {
                    format!(" ({} applied before it)", done.join(", "))
                };
                return Err(Error::new(format!(
                    "changing \"{title}\" failed: {e}{before}"
                )));
            }
            done.push(format!("\"{title}\""));
        }
        Ok(json!({"message": format!("Applied {} change(s) to the work graph.", done.len())}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(grants: Vec<Grant>) -> SessionContext {
        SessionContext {
            session_id: Uuid::now_v7().to_string(),
            owner: "dev".into(),
            grants,
            role: Default::default(),
        }
    }

    async fn granted_at(db: &PgPool, graph: Uuid, level: &str) -> SessionContext {
        let grant = grant(db, "dev", &json!({"graph": graph, "level": level}))
            .await
            .unwrap();
        session(vec![grant])
    }

    async fn update(
        db: &PgPool,
        session: &SessionContext,
        arguments: Value,
    ) -> std::result::Result<Value, String> {
        call(db, session, UPDATE, &arguments)
            .await
            .map(|text| serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn each_level_does_what_it_covers_and_no_more(db: PgPool) {
        let graph = super::super::create_graph(&db, "dev", "g")
            .await
            .unwrap()
            .id;
        // Only the owner's graphs can be granted.
        let elsewhere = super::super::create_graph(&db, "eve", "g")
            .await
            .unwrap()
            .id;
        assert!(
            grant(&db, "dev", &json!({"graph": elsewhere}))
                .await
                .is_err()
        );
        assert_eq!(
            grant(&db, "dev", &json!({"graph": graph}))
                .await
                .unwrap()
                .param("level"),
            Some("track"),
            "track by default"
        );

        let reader = granted_at(&db, graph, "read").await;
        let tracker = granted_at(&db, graph, "track").await;
        let planner = granted_at(&db, graph, "plan").await;
        let names = |s: &SessionContext| {
            tools(s)
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&reader), [LIST]);
        assert_eq!(names(&tracker), [LIST, UPDATE]);
        assert!(names(&session(vec![])).is_empty());
        assert!(shown(&planner, PROPOSE_PLAN) && !shown(&tracker, PROPOSE_PLAN));

        let add = json!({"action": "add_item", "title": "fix"});
        assert!(
            update(&db, &reader, add.clone())
                .await
                .unwrap_err()
                .contains("read only")
        );
        let fix = update(&db, &tracker, add).await.unwrap();
        let fix_id = fix["id"].as_str().unwrap().to_owned();
        let listed = call(&db, &reader, LIST, &json!({"view": "frontier"}))
            .await
            .unwrap();
        assert!(listed.contains("\"fix\""), "{listed}");

        // Tracking: a link to this session makes it active; done by hand isn't allowed while a completing link
        // decides, but is once it's related only.
        let linked = update(
            &db,
            &tracker,
            json!({"action": "link", "item": fix_id, "kind": "pull_request", "target": "o/r#1", "role": "completes"}),
        )
        .await
        .unwrap();
        assert_eq!(
            super::super::item(&db, "dev", fix_id.parse().unwrap())
                .await
                .unwrap()
                .unwrap()
                .state,
            "active"
        );
        let finished = update(
            &db,
            &tracker,
            json!({"action": "link_status", "link": linked["id"], "status": "finished"}),
        )
        .await
        .unwrap();
        assert_eq!(finished["status"], "finished");
        assert_eq!(
            super::super::item(&db, "dev", fix_id.parse().unwrap())
                .await
                .unwrap()
                .unwrap()
                .state,
            "done"
        );

        // Dropping and reopening are plan changes, at any level, through a proposal.
        for level in [&tracker, &planner] {
            let dropped = update(
                &db,
                level,
                json!({"action": "set_state", "item": fix_id, "state": "dropped"}),
            )
            .await;
            assert!(dropped.unwrap_err().contains("plan change"));
        }
        let plan = WorkPlan::new(db.clone());
        let changes =
            json!({"changes": [{"item": fix_id, "action": "drop"}], "reason": "superseded"});
        assert!(plan.prepare(&tracker, changes.clone()).await.is_err());
        let payload = plan.prepare(&planner, changes).await.unwrap();
        assert_eq!(
            plan.target(&payload).as_deref(),
            Some(graph.to_string().as_str())
        );
        assert_eq!(payload["changes"][0]["title"], "fix");
        assert_eq!(plan.summary(&payload), "1 change to the work graph g");

        // A grant of one graph reaches nothing in another.
        let theirs = super::super::create_graph(&db, "dev", "other")
            .await
            .unwrap()
            .id;
        let other = super::super::create_item(
            &db,
            Actor {
                user: "dev",
                session: None,
            },
            theirs,
            &NewItem {
                title: "x".into(),
                description: String::new(),
                parent_id: None,
                after: None,
            },
        )
        .await
        .unwrap();
        let reach = update(
            &db,
            &tracker,
            json!({"action": "edit_item", "item": other.id, "title": "y"}),
        )
        .await;
        assert!(reach.unwrap_err().contains("no such item"));
        let named = update(
            &db,
            &tracker,
            json!({"graph": theirs, "action": "add_item", "title": "y"}),
        )
        .await;
        assert!(named.unwrap_err().contains("no work grant"));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn an_approved_plan_moves_drops_and_reopens(db: PgPool) {
        let graph = super::super::create_graph(&db, "dev", "g")
            .await
            .unwrap()
            .id;
        let planner = granted_at(&db, graph, "plan").await;
        let add =
            |title: &str| update(&db, &planner, json!({"action": "add_item", "title": title}));
        let first = add("first").await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let second = add("second").await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let plan = WorkPlan::new(db.clone());
        let payload = plan
            .prepare(
                &planner,
                json!({
                    "changes": [
                        {"item": second, "action": "move", "after": null},
                        {"item": first, "action": "drop"},
                    ],
                    "reason": "reorder",
                }),
            )
            .await
            .unwrap();
        let detail = plan.detail(&planner, &payload).await.unwrap();
        assert_eq!(detail["current"].as_array().unwrap().len(), 2);
        plan.execute(&planner, Uuid::now_v7(), &payload)
            .await
            .unwrap();
        let ready = call(&db, &planner, LIST, &json!({"view": "frontier"}))
            .await
            .unwrap();
        let ready: Vec<Value> = serde_json::from_str(&ready).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0]["title"], "second");
        // Its changes are recorded as the session's.
        let history = super::super::item_history(&db, "dev", first.parse().unwrap())
            .await
            .unwrap()
            .unwrap();
        let dropped = history.last().unwrap();
        assert_eq!(
            dropped.session_id.map(|s| s.to_string()),
            Some(planner.session_id.clone())
        );

        let reopen = plan
            .prepare(
                &planner,
                json!({"changes": [{"item": first, "action": "reopen"}], "reason": "back"}),
            )
            .await
            .unwrap();
        plan.execute(&planner, Uuid::now_v7(), &reopen)
            .await
            .unwrap();
        assert_eq!(
            super::super::item(&db, "dev", first.parse().unwrap())
                .await
                .unwrap()
                .unwrap()
                .state,
            "open"
        );
    }

    #[test]
    fn a_sub_session_tracks_at_most() {
        let mut plan = Grant::new(capability::WORK, json!({"graph": "g", "level": "plan"}));
        inherited(&mut plan);
        assert_eq!(plan.param("level"), Some("track"));
        let mut read = Grant::new(capability::WORK, json!({"graph": "g", "level": "read"}));
        inherited(&mut read);
        assert_eq!(read.param("level"), Some("read"));
    }

    #[sqlx::test(migrator = "crate::db::MIGRATOR")]
    async fn a_session_is_told_its_graph_and_the_items_it_works_on(db: PgPool) {
        let graph = super::super::create_graph(&db, "dev", "sloppod")
            .await
            .unwrap()
            .id;
        let tracker = granted_at(&db, graph, "track").await;
        let item = update(
            &db,
            &tracker,
            json!({"action": "add_item", "title": "stage 2"}),
        )
        .await
        .unwrap();
        assert!(prompt(&db, &session(vec![])).await.is_none());
        let told = prompt(&db, &tracker).await.unwrap();
        assert!(
            told.contains("`sloppod`") && told.contains("view: \"frontier\""),
            "{told}"
        );
        assert!(!told.contains("stage 2"));
        // Linked (here directly, since the session isn't a real one), it's told the item.
        sqlx::query(
            "insert into work_links (id, item_id, kind, target, role) values ($1, $2, 'session', $3, 'related')",
        )
        .bind(Uuid::now_v7())
        .bind(item["id"].as_str().unwrap().parse::<Uuid>().unwrap())
        .bind(&tracker.session_id)
        .execute(&db)
        .await
        .unwrap();
        let told = prompt(&db, &tracker).await.unwrap();
        assert!(told.contains("- stage 2 (open, in `sloppod`"), "{told}");
    }
}
