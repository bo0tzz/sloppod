//! The work graph's rules (ADR 30): which edges may be added, what state an item's links and children put it in,
//! which moves by hand are allowed, and what's ready. Pure, over a snapshot of one graph, so every caller (the owner's
//! API, and later agents' tools and the watchers that move links) applies the same ones.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Open,
    Active,
    Done,
    Dropped,
}

impl State {
    /// Done or dropped: what releases the items it blocks, and lets its parent finish.
    pub fn settled(self) -> bool {
        matches!(self, State::Done | State::Dropped)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            State::Open => "open",
            State::Active => "active",
            State::Done => "done",
            State::Dropped => "dropped",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => State::Open,
            "active" => State::Active,
            "done" => State::Done,
            "dropped" => State::Dropped,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Issue,
    PullRequest,
    Session,
    Proposal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Finishing it finishes the item.
    Completes,
    Related,
}

/// Where what a link points at stands: `open` while it's under way (a pull request or issue open, a session working,
/// a proposal waiting), `finished` once it's done what it's for (merged, closed as completed, reported done, carried
/// out), `abandoned` if it ended without that (closed unmerged or as not planned, rejected).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkStatus {
    Open,
    Finished,
    Abandoned,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: Uuid,
    pub parent: Option<Uuid>,
    pub state: State,
    pub rank: f64,
}

#[derive(Debug, Clone)]
pub struct Link {
    pub item: Uuid,
    pub kind: LinkKind,
    pub role: Role,
    pub status: LinkStatus,
}

/// One graph's items, `blocks` edges (blocker, blocked) and links.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub items: HashMap<Uuid, Item>,
    pub blocks: Vec<(Uuid, Uuid)>,
    pub links: Vec<Link>,
}

impl Graph {
    pub fn new(items: Vec<Item>, blocks: Vec<(Uuid, Uuid)>, links: Vec<Link>) -> Self {
        Graph {
            items: items.into_iter().map(|item| (item.id, item)).collect(),
            blocks,
            links,
        }
    }

    pub fn children(&self, id: Uuid) -> impl Iterator<Item = &Item> {
        self.items
            .values()
            .filter(move |item| item.parent == Some(id))
    }

    /// The item's parent, its parent's parent, and so on.
    pub fn ancestors(&self, id: Uuid) -> Vec<Uuid> {
        let mut chain = Vec::new();
        let mut at = self.items.get(&id).and_then(|item| item.parent);
        while let Some(parent) = at {
            if chain.contains(&parent) {
                break;
            }
            chain.push(parent);
            at = self.items.get(&parent).and_then(|item| item.parent);
        }
        chain
    }

    /// What has to finish before an item can: what blocks it, and its children. Both kinds of edge are kept acyclic
    /// together, so no item ends up waiting on itself however the two mix.
    fn waits_on(&self, id: Uuid) -> Vec<Uuid> {
        self.blocks
            .iter()
            .filter(|(_, blocked)| *blocked == id)
            .map(|(blocker, _)| *blocker)
            .chain(self.children(id).map(|child| child.id))
            .collect()
    }

    /// Whether `from` waits on `to`, directly or through others.
    fn waits_for(&self, from: Uuid, to: Uuid) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![from];
        while let Some(at) = stack.pop() {
            if at == to {
                return true;
            }
            if seen.insert(at) {
                stack.extend(self.waits_on(at));
            }
        }
        false
    }

    /// Why `blocker` can't be made to block `blocked`, if it can't: it would close a cycle.
    pub fn refuse_block(&self, blocker: Uuid, blocked: Uuid) -> Option<String> {
        if blocker == blocked {
            return Some("an item can't block itself".into());
        }
        // `blocked` will wait on `blocker`, so `blocker` mustn't already wait on `blocked`.
        self.waits_for(blocker, blocked).then(|| {
            "that would close a cycle: the blocker already waits on the item it would block".into()
        })
    }

    /// Why `item` can't be made part of `parent`, if it can't: it would close a cycle.
    pub fn refuse_parent(&self, item: Uuid, parent: Uuid) -> Option<String> {
        if item == parent {
            return Some("an item can't be part of itself".into());
        }
        // `parent` will wait on `item`, so `item` mustn't already wait on `parent`.
        self.waits_for(item, parent).then(|| {
            "that would close a cycle: the item already waits on the one it would be part of".into()
        })
    }

    fn children_settled(&self, id: Uuid) -> bool {
        self.children(id).all(|child| child.state.settled())
    }

    fn has_children(&self, id: Uuid) -> bool {
        self.children(id).next().is_some()
    }

    /// The item's completing links that still count: a completing link that was abandoned doesn't finish the item.
    fn completing(&self, id: Uuid) -> impl Iterator<Item = &Link> {
        self.links.iter().filter(move |link| {
            link.item == id && link.role == Role::Completes && link.status != LinkStatus::Abandoned
        })
    }

    /// Whether what's linked finishes the item: every child is done or dropped, and every completing link is
    /// finished; an item without completing links finishes by its children, if it has any.
    fn finished(&self, id: Uuid) -> bool {
        if !self.children_settled(id) {
            return false;
        }
        let mut completing = self.completing(id).peekable();
        if completing.peek().is_none() {
            return self.has_children(id);
        }
        completing.all(|link| link.status == LinkStatus::Finished)
    }

    /// Whether something linked is working on the item: a session, or a completing pull request or proposal that's
    /// open.
    fn worked_on(&self, id: Uuid) -> bool {
        self.links.iter().any(|link| {
            link.item == id
                && link.status == LinkStatus::Open
                && (link.kind == LinkKind::Session
                    || (link.role == Role::Completes
                        && matches!(link.kind, LinkKind::PullRequest | LinkKind::Proposal)))
        })
    }

    /// The state an item's links and children put it in. A dropped item stays dropped; one they finish is done; one
    /// they made done and no longer finish goes back to active or open; one something works on is active. Otherwise
    /// it keeps the state it was given by hand.
    pub fn settle(&self, id: Uuid) -> State {
        let Some(item) = self.items.get(&id) else {
            return State::Open;
        };
        if item.state == State::Dropped {
            return State::Dropped;
        }
        if self.finished(id) {
            return State::Done;
        }
        let follows_links = self.completing(id).next().is_some() || self.has_children(id);
        if self.worked_on(id) {
            State::Active
        } else if item.state == State::Done && follows_links {
            State::Open
        } else {
            item.state
        }
    }

    /// Why an item can't be moved to `to` by hand, if it can't.
    pub fn refuse_move(&self, id: Uuid, to: State) -> Option<String> {
        let item = self.items.get(&id)?;
        if item.state == to || to == State::Dropped || item.state == State::Dropped {
            return None;
        }
        let unfinished = self
            .children(id)
            .filter(|child| !child.state.settled())
            .count();
        match to {
            State::Done if unfinished > 0 => Some(format!(
                "{unfinished} of its parts aren't done or dropped yet"
            )),
            State::Done if self.completing(id).next().is_some() => Some(
                "it's done when its completing links are finished; change those, or the link's role".into(),
            ),
            State::Open | State::Active if self.finished(id) => Some(
                "what's linked to it is finished, so it's done; reopen a link or add a part instead".into(),
            ),
            State::Open if self.worked_on(id) => {
                Some("a session or an open pull request is working on it".into())
            }
            _ => None,
        }
    }

    /// Whether something unfinished blocks the item, or one of the items it's part of.
    fn blocked(&self, id: Uuid) -> bool {
        std::iter::once(id).chain(self.ancestors(id)).any(|at| {
            self.blocks.iter().any(|(blocker, blocked)| {
                *blocked == at && self.items.get(blocker).is_some_and(|b| !b.state.settled())
            })
        })
    }

    /// The ranks from the item's outermost ancestor down to it, which order items across the graph.
    fn rank_path(&self, id: Uuid) -> Vec<f64> {
        let mut path: Vec<f64> = self
            .ancestors(id)
            .iter()
            .rev()
            .filter_map(|a| self.items.get(a).map(|item| item.rank))
            .collect();
        path.extend(self.items.get(&id).map(|item| item.rank));
        path
    }

    /// What's ready to start, in order: open items that nothing unfinished blocks (nor any item they're part of),
    /// whose parts are all done or dropped, and that no done or dropped item contains. Ranks order siblings, and
    /// across the graph an item comes where its ancestors do.
    pub fn frontier(&self) -> Vec<Uuid> {
        let mut ready: Vec<(Vec<f64>, Uuid)> = self
            .items
            .values()
            .filter(|item| item.state == State::Open)
            .filter(|item| self.children_settled(item.id))
            .filter(|item| {
                self.ancestors(item.id)
                    .iter()
                    .all(|a| self.items.get(a).is_some_and(|a| !a.state.settled()))
            })
            .filter(|item| !self.blocked(item.id))
            .map(|item| (self.rank_path(item.id), item.id))
            .collect();
        ready.sort_by(|(a, x), (b, y)| {
            a.partial_cmp(b)
                .unwrap_or(Ordering::Equal)
                .then_with(|| x.cmp(y))
        });
        ready.into_iter().map(|(_, id)| id).collect()
    }
}

/// A rank between two neighbours, either of which may be missing (the start or end of the siblings), or `None` when
/// there's no room left between them and the siblings need spreading out.
pub fn rank_between(before: Option<f64>, after: Option<f64>) -> Option<f64> {
    let rank = match (before, after) {
        (None, None) => 1.0,
        (Some(b), None) => b.floor() + 1.0,
        (None, Some(a)) => a.ceil() - 1.0,
        (Some(b), Some(a)) => b + (a - b) / 2.0,
    };
    let fits = before.is_none_or(|b| rank > b) && after.is_none_or(|a| rank < a);
    fits.then_some(rank)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn item(n: u128, parent: Option<u128>, state: State, rank: f64) -> Item {
        Item {
            id: id(n),
            parent: parent.map(id),
            state,
            rank,
        }
    }

    fn link(n: u128, kind: LinkKind, role: Role, status: LinkStatus) -> Link {
        Link {
            item: id(n),
            kind,
            role,
            status,
        }
    }

    use LinkKind::*;
    use LinkStatus::{Abandoned, Finished};
    use Role::*;
    use State::*;

    #[test]
    fn edges_never_close_a_cycle() {
        // 1 blocks 2, 2 blocks 3; 4 is part of 5.
        let graph = Graph::new(
            vec![
                item(1, None, Open, 1.0),
                item(2, None, Open, 2.0),
                item(3, None, Open, 3.0),
                item(4, Some(5), Open, 1.0),
                item(5, None, Open, 4.0),
            ],
            vec![(id(1), id(2)), (id(2), id(3))],
            vec![],
        );
        assert!(graph.refuse_block(id(3), id(1)).is_some());
        assert!(graph.refuse_block(id(1), id(1)).is_some());
        assert!(graph.refuse_block(id(1), id(3)).is_none());
        // A parent blocking its own part would never finish.
        assert!(graph.refuse_block(id(5), id(4)).is_some());
        assert!(graph.refuse_block(id(4), id(5)).is_none());
        // Making 5 part of 4, its own part, or 3 part of 1, which 3 waits on.
        assert!(graph.refuse_parent(id(5), id(4)).is_some());
        assert!(graph.refuse_parent(id(3), id(1)).is_some());
        assert!(graph.refuse_parent(id(1), id(3)).is_none());
        assert!(graph.refuse_parent(id(5), id(5)).is_some());
    }

    #[test]
    fn links_and_parts_finish_an_item() {
        let pr = |status| link(1, PullRequest, Completes, status);
        let graph = |links, children: Vec<Item>| {
            let mut items = vec![item(1, None, Open, 1.0)];
            items.extend(children);
            Graph::new(items, vec![], links)
        };
        // A completing pull request: active while open, done once merged; closed unmerged, it stops counting.
        assert_eq!(
            graph(vec![pr(LinkStatus::Open)], vec![]).settle(id(1)),
            Active
        );
        assert_eq!(graph(vec![pr(Finished)], vec![]).settle(id(1)), Done);
        assert_eq!(graph(vec![pr(Abandoned)], vec![]).settle(id(1)), Open);
        // Every completing link has to finish.
        let issue = link(1, Issue, Completes, LinkStatus::Open);
        assert_eq!(graph(vec![pr(Finished), issue], vec![]).settle(id(1)), Open);
        // A related link finishes nothing; a related session still makes it active.
        let related = link(1, PullRequest, Related, Finished);
        assert_eq!(graph(vec![related], vec![]).settle(id(1)), Open);
        let session = link(1, Session, Related, LinkStatus::Open);
        assert_eq!(graph(vec![session], vec![]).settle(id(1)), Active);
        // Parts: done once each is done or dropped, and with them its own completing links.
        let parts = |a, b| vec![item(2, Some(1), a, 1.0), item(3, Some(1), b, 2.0)];
        assert_eq!(graph(vec![], parts(Done, Open)).settle(id(1)), Open);
        assert_eq!(graph(vec![], parts(Done, Dropped)).settle(id(1)), Done);
        assert_eq!(
            graph(vec![pr(LinkStatus::Open)], parts(Done, Done)).settle(id(1)),
            Active
        );
        assert_eq!(
            graph(vec![pr(Finished)], parts(Done, Done)).settle(id(1)),
            Done
        );
        // A done parent that gains an unfinished part opens again.
        let mut reopened = graph(vec![], parts(Done, Open));
        reopened.items.get_mut(&id(1)).unwrap().state = Done;
        assert_eq!(reopened.settle(id(1)), Open);
        // One with nothing linked stays as it was set by hand; a dropped one stays dropped.
        let mut by_hand = graph(vec![], vec![]);
        by_hand.items.get_mut(&id(1)).unwrap().state = Done;
        assert_eq!(by_hand.settle(id(1)), Done);
        by_hand.items.get_mut(&id(1)).unwrap().state = Dropped;
        by_hand.links.push(pr(Finished));
        assert_eq!(by_hand.settle(id(1)), Dropped);
    }

    #[test]
    fn moves_by_hand_keep_to_the_rules() {
        let graph = Graph::new(
            vec![
                item(1, None, Open, 1.0),
                item(2, Some(1), Open, 1.0),
                item(3, None, Open, 2.0),
                item(4, None, Done, 3.0),
                item(5, Some(4), Done, 1.0),
                item(6, None, Active, 4.0),
            ],
            vec![],
            vec![
                link(3, PullRequest, Completes, LinkStatus::Open),
                link(6, Session, Related, LinkStatus::Open),
            ],
        );
        // Not done while a part is unfinished, or while completing links decide it.
        assert!(graph.refuse_move(id(1), Done).is_some());
        assert!(graph.refuse_move(id(3), Done).is_some());
        assert!(graph.refuse_move(id(2), Done).is_none());
        // Not reopened while what's linked finishes it.
        assert!(graph.refuse_move(id(4), Open).is_some());
        // Not back to open while something works on it; active is fine.
        assert!(graph.refuse_move(id(6), Open).is_some());
        assert!(graph.refuse_move(id(1), Active).is_none());
        // Dropping, and taking back a drop, are always allowed.
        assert!(graph.refuse_move(id(1), Dropped).is_none());
    }

    #[test]
    fn the_frontier_is_what_nothing_unfinished_blocks_by_rank() {
        // 1 blocks 2. 3 contains 4 and 5, ranked 5 first; 6 blocks 3, so its parts wait too until it's dropped.
        let mut graph = Graph::new(
            vec![
                item(1, None, Open, 2.0),
                item(2, None, Open, 1.0),
                item(3, None, Open, 3.0),
                item(4, Some(3), Open, 2.0),
                item(5, Some(3), Open, 1.0),
                item(6, None, Active, 4.0),
                item(7, None, Done, 0.5),
                item(8, Some(7), Open, 1.0),
            ],
            vec![(id(1), id(2)), (id(6), id(3))],
            vec![],
        );
        // 2 is blocked; 3's parts wait on 6; 3 has unfinished parts; 6 is under way; 8 is in a done item.
        assert_eq!(graph.frontier(), [id(1)]);
        graph.items.get_mut(&id(6)).unwrap().state = Dropped;
        assert_eq!(graph.frontier(), [id(1), id(5), id(4)]);
        graph.items.get_mut(&id(1)).unwrap().state = Done;
        // 2 ranks first among the top level.
        assert_eq!(graph.frontier(), [id(2), id(5), id(4)]);
    }

    #[test]
    fn ranks_fit_between_neighbours() {
        assert_eq!(rank_between(None, None), Some(1.0));
        assert_eq!(rank_between(Some(2.5), None), Some(3.0));
        assert_eq!(rank_between(None, Some(1.0)), Some(0.0));
        assert_eq!(rank_between(Some(1.0), Some(2.0)), Some(1.5));
        let tight = 1.0 + f64::EPSILON;
        assert_eq!(rank_between(Some(1.0), Some(tight)), None);
    }
}
