// The runtime's work graphs (ADR 30, `docs/api.md` "Work"), in memory: graphs, items, edges, links and their history,
// with the rules of `work/rules.rs` (what settles an item, which moves by hand are refused, what's ready) and undo.
import { randomUUID } from 'node:crypto';

const now = () => new Date().toISOString();
const settled = (state) => state === 'done' || state === 'dropped';

/** The user's work graphs. The first is the one stage 2's grants tests pick. */
export const workGraphs = [{ id: '0192f000-0000-7000-8000-000000000001', name: 'sloppod', created_at: '2026-10-29T10:00:00Z' }];
const items = new Map();
/** `blocks` edges: [blocker, blocked]. */
let edges = [];
const links = new Map();
const events = [];

const graphItems = (graph) => [...items.values()].filter((item) => item.graph_id === graph);
const children = (id) => [...items.values()].filter((item) => item.parent_id === id);
const itemLinks = (id) => [...links.values()].filter((link) => link.item_id === id);

function ancestors(id) {
  const chain = [];
  let at = items.get(id)?.parent_id;
  while (at && !chain.includes(at)) {
    chain.push(at);
    at = items.get(at)?.parent_id;
  }
  return chain;
}

/** What has to finish before an item can: what blocks it or an item it's part of, and its parts. */
function waitsOn(id, edgeList = edges, parentOf = (i) => items.get(i)?.parent_id) {
  const chain = [id];
  for (let at = parentOf(id); at && !chain.includes(at); at = parentOf(at)) chain.push(at);
  return [
    ...edgeList.filter(([, blocked]) => chain.includes(blocked)).map(([blocker]) => blocker),
    ...[...items.values()].filter((item) => parentOf(item.id) === id).map((item) => item.id),
  ];
}

function deadlocked(edgeList, parentOf) {
  const colour = new Map();
  const visit = (id) => {
    if (colour.get(id) === 'grey') return true;
    if (colour.get(id) === 'black') return false;
    colour.set(id, 'grey');
    for (const next of waitsOn(id, edgeList, parentOf)) if (visit(next)) return true;
    colour.set(id, 'black');
    return false;
  };
  return [...items.keys()].some((id) => visit(id));
}

const completing = (id) => itemLinks(id).filter((l) => l.role === 'completes' && l.status !== 'abandoned');
function finished(id) {
  if (!children(id).every((child) => settled(child.state))) return false;
  const counting = completing(id);
  if (counting.length === 0) return children(id).length > 0;
  return counting.every((l) => l.status === 'finished');
}
const workedOn = (id) =>
  itemLinks(id).some(
    (l) => l.status === 'open' && (l.kind === 'session' || (l.role === 'completes' && ['pull_request', 'proposal'].includes(l.kind))),
  );

function settle(id) {
  const item = items.get(id);
  if (item.state === 'dropped') return 'dropped';
  if (finished(id)) return 'done';
  if (workedOn(id)) return 'active';
  const followsLinks = completing(id).length > 0 || children(id).length > 0;
  if ((item.state === 'done' || item.state === 'active') && item.by_rules) return 'open';
  if (item.state === 'done' && followsLinks) return 'open';
  return item.state;
}

function refuseMove(id, to) {
  const item = items.get(id);
  if (item.state === to || to === 'dropped' || item.state === 'dropped') return null;
  const unfinished = children(id).filter((child) => !settled(child.state)).length;
  if (to === 'done' && unfinished > 0) return `${unfinished} of its parts aren't done or dropped yet`;
  if (to === 'done' && completing(id).length > 0) return "it's done when its completing links are finished; change those, or the link's role";
  if ((to === 'open' || to === 'active') && finished(id)) return "what's linked to it is finished, so it's done; reopen a link or add a part instead";
  if (to === 'open' && workedOn(id)) return 'a session or an open pull request is working on it';
  return null;
}

function record(graph, item, actor, kind, before, after, undoes = null, automatic = false) {
  const event = { id: randomUUID(), graph_id: graph, item_id: item, actor, session_id: null, kind, before, after, automatic, undoes, at: now() };
  events.push(event);
  return event;
}

function setState(id, state, byRules) {
  const item = items.get(id);
  item.state = state;
  item.by_rules = byRules;
  item.updated_at = now();
}

function settleFrom(graph, actor, ids) {
  for (const start of ids) {
    for (const id of [start, ...ancestors(start)]) {
      if (!items.has(id)) continue;
      const was = items.get(id).state;
      const to = settle(id);
      if (to !== was) {
        setState(id, to, true);
        record(graph, id, actor, 'state', { state: was }, { state: to }, null, true);
      }
    }
  }
}

/** A rank right after `after` among `parent`'s parts (`null`: first, `undefined`: last). */
function place(graph, parent, moving, after) {
  const siblings = graphItems(graph)
    .filter((item) => item.parent_id === parent && item.id !== moving)
    .sort((a, b) => a.rank - b.rank);
  let index = siblings.length;
  if (after === null) index = 0;
  else if (after !== undefined) {
    const at = siblings.findIndex((item) => item.id === after);
    if (at < 0) throw [400, '`after` names an item that isn\'t among its new siblings'];
    index = at + 1;
  }
  const before = siblings[index - 1]?.rank;
  const next = siblings[index]?.rank;
  if (before === undefined && next === undefined) return 1;
  if (next === undefined) return Math.floor(before) + 1;
  if (before === undefined) return Math.ceil(next) - 1;
  return before + (next - before) / 2;
}

function view(item) {
  const { by_rules: _, ...rest } = item;
  return {
    ...rest,
    links: itemLinks(item.id),
    blocks: edges.filter(([blocker]) => blocker === item.id).map(([, blocked]) => blocked),
    blocked_by: edges.filter(([, blocked]) => blocked === item.id).map(([blocker]) => blocker),
  };
}

function rankPath(id) {
  return [...ancestors(id).reverse(), id].map((at) => items.get(at).rank);
}

function frontier(graph) {
  const blocked = (id) => [id, ...ancestors(id)].some((at) => edges.some(([b, x]) => x === at && !settled(items.get(b)?.state)));
  return graphItems(graph)
    .filter((item) => item.state === 'open')
    .filter((item) => children(item.id).every((child) => settled(child.state)))
    .filter((item) => ancestors(item.id).every((a) => !settled(items.get(a).state)))
    .filter((item) => !blocked(item.id))
    .sort((a, b) => {
      const [x, y] = [rankPath(a.id), rankPath(b.id)];
      for (let i = 0; i < Math.min(x.length, y.length); i++) if (x[i] !== y[i]) return x[i] - y[i];
      return x.length - y.length || a.id.localeCompare(b.id);
    });
}

function checkTarget(kind, target) {
  if (kind === 'issue' || kind === 'pull_request') {
    if (!/^[A-Za-z0-9-]{1,39}\/[A-Za-z0-9._-]{1,100}#[1-9]\d*$/.test(target)) throw [400, 'an issue or pull request is named owner/repo#number'];
    return target;
  }
  if (kind === 'session' || kind === 'proposal') {
    if (!/^[0-9a-f-]{36}$/i.test(target)) throw [400, 'a session or proposal is named by its id'];
    return target.toLowerCase();
  }
  throw [400, '`kind` is issue, pull_request, session or proposal'];
}

const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

/** Handles `/api/v1/work/…`; `user` is who asks. */
export async function handleWork(req, res, parts, { json, readBody, user }) {
  const method = req.method;
  const actor = user;
  try {
    const out = await route();
    if (out === undefined) return json(res, 404, { error: 'not found' });
    const [status, body] = out;
    if (status === 204) {
      res.writeHead(204);
      return res.end();
    }
    return json(res, status, body);
  } catch (e) {
    if (Array.isArray(e)) return json(res, e[0], { error: e[1] });
    throw e;
  }

  async function route() {
    const [, collection, id, sub, other] = parts;
    if (collection === 'graphs') {
      if (!id) {
        if (method === 'GET') return [200, workGraphs];
        if (method === 'POST') {
          const name = String((await readBody(req)).name ?? '').trim();
          if (!name || name.length > 100) throw [400, "a graph's name is 1 to 100 characters"];
          if (workGraphs.some((g) => g.name === name)) throw [409, `you have a graph called ${name} already`];
          const graph = { id: randomUUID(), name, created_at: now() };
          workGraphs.push(graph);
          return [201, graph];
        }
        return;
      }
      const graph = workGraphs.find((g) => g.id === id);
      if (!graph) return;
      if (!sub) {
        if (method === 'GET') return [200, { ...graph, items: graphItems(id).sort((a, b) => a.rank - b.rank).map(view) }];
        if (method === 'PATCH') {
          const name = String((await readBody(req)).name ?? '').trim();
          if (!name) throw [400, "a graph's name is 1 to 100 characters"];
          record(id, null, actor, 'rename', { name: graph.name }, { name });
          graph.name = name;
          return [200, graph];
        }
        if (method === 'DELETE') {
          workGraphs.splice(workGraphs.indexOf(graph), 1);
          return [204];
        }
        return;
      }
      if (sub === 'frontier' && method === 'GET') return [200, frontier(id).map(view)];
      if (sub === 'history' && method === 'GET') return [200, events.filter((e) => e.graph_id === id)];
      if (sub === 'items' && method === 'POST') {
        const body = await readBody(req);
        const title = String(body.title ?? '').trim();
        if (!title || title.length > 200) throw [400, 'a title is 1 to 200 characters'];
        const parent = body.parent_id ?? null;
        if (parent && items.get(parent)?.graph_id !== id) throw [400, "parent_id isn't an item in this graph"];
        const item = {
          id: randomUUID(),
          graph_id: id,
          parent_id: parent,
          title,
          description: String(body.description ?? '').trim(),
          state: 'open',
          rank: place(id, parent, null, 'after' in body ? body.after : undefined),
          by_rules: false,
          created_at: now(),
          updated_at: now(),
        };
        items.set(item.id, item);
        record(id, item.id, actor, 'create', null, { title, description: item.description, parent_id: parent, rank: item.rank });
        settleFrom(id, actor, [item.id]);
        return [201, view(item)];
      }
      return;
    }
    if (collection === 'items') {
      const item = items.get(id);
      if (!item || !workGraphs.some((g) => g.id === item.graph_id)) return;
      const graph = item.graph_id;
      if (!sub) {
        if (method === 'GET') return [200, view(item)];
        if (method === 'PATCH') {
          const body = await readBody(req);
          const before = { title: item.title, description: item.description };
          const after = { title: body.title?.trim() ?? item.title, description: body.description?.trim() ?? item.description };
          if (!after.title) throw [400, 'a title is 1 to 200 characters'];
          if (!same(before, after)) {
            Object.assign(item, after, { updated_at: now() });
            record(graph, id, actor, 'edit', before, after);
          }
          return [200, view(item)];
        }
        return;
      }
      if (sub === 'state' && method === 'POST') {
        const to = (await readBody(req)).state;
        if (!['open', 'active', 'done', 'dropped'].includes(to)) throw [400, '`state` is open, active, done or dropped'];
        const why = refuseMove(id, to);
        if (why) throw [409, why];
        if (item.state !== to) {
          const was = item.state;
          setState(id, to, false);
          record(graph, id, actor, 'state', { state: was }, { state: to });
          settleFrom(graph, actor, [id]);
        }
        return [200, view(item)];
      }
      if (sub === 'move' && method === 'POST') {
        const body = await readBody(req);
        const parent = 'parent_id' in body ? body.parent_id : item.parent_id;
        if (parent && parent !== item.parent_id) {
          if (items.get(parent)?.graph_id !== graph) throw [400, "parent_id isn't an item in this graph"];
          if (parent === id || ancestors(parent).includes(id)) throw [409, "an item can't be part of itself or of one of its parts"];
          const parentOf = (i) => (i === id ? parent : items.get(i)?.parent_id);
          if (deadlocked(edges, parentOf)) throw [409, 'that would close a cycle'];
        }
        const rank = place(graph, parent, id, 'after' in body ? body.after : undefined);
        const before = { parent_id: item.parent_id, rank: item.rank };
        const oldParent = item.parent_id;
        Object.assign(item, { parent_id: parent, rank, updated_at: now() });
        record(graph, id, actor, 'move', before, { parent_id: parent, rank });
        settleFrom(graph, actor, [id, ...(oldParent ? [oldParent] : [])]);
        return [200, view(item)];
      }
      if (sub === 'blocks' && method === 'POST' && !other) {
        const blocked = (await readBody(req)).item;
        if (items.get(blocked)?.graph_id !== graph) throw [400, "the item to block isn't in this graph"];
        if (blocked === id) throw [409, "an item can't block itself"];
        if (!edges.some(([b, x]) => b === id && x === blocked)) {
          if (deadlocked([...edges, [id, blocked]])) throw [409, 'that would close a cycle: the blocker already waits on the item it would block, or on what blocks it'];
          edges.push([id, blocked]);
          record(graph, blocked, actor, 'block', null, { blocker: id, blocked });
        }
        return [200, view(item)];
      }
      if (sub === 'blocks' && method === 'DELETE' && other) {
        const before = edges.length;
        edges = edges.filter(([b, x]) => !(b === id && x === other));
        if (edges.length === before) return;
        record(graph, other, actor, 'unblock', { blocker: id, blocked: other }, null);
        return [200, view(item)];
      }
      if (sub === 'links' && method === 'POST') {
        const body = await readBody(req);
        const target = checkTarget(body.kind, String(body.target ?? '').trim());
        if (!['completes', 'related'].includes(body.role)) throw [400, '`role` is completes or related'];
        if (itemLinks(id).some((l) => l.kind === body.kind && l.target === target)) throw [409, 'the item links that already'];
        const link = { id: randomUUID(), item_id: id, kind: body.kind, target, role: body.role, status: 'open', created_at: now() };
        links.set(link.id, link);
        record(graph, id, actor, 'link', null, link);
        settleFrom(graph, actor, [id]);
        return [201, link];
      }
      if (sub === 'history' && method === 'GET') {
        return [200, events.filter((e) => e.item_id === id || ((e.kind === 'block' || e.kind === 'unblock') && (e.after ?? e.before)?.blocker === id))];
      }
      return;
    }
    if (collection === 'links') {
      const link = links.get(id);
      if (!link) return;
      const graph = items.get(link.item_id).graph_id;
      if (method === 'PATCH') {
        const status = (await readBody(req)).status;
        if (!['open', 'finished', 'abandoned'].includes(status)) throw [400, '`status` is open, finished or abandoned'];
        if (link.status !== status) {
          record(graph, link.item_id, actor, 'link_status', { link: id, status: link.status }, { link: id, status });
          link.status = status;
          settleFrom(graph, actor, [link.item_id]);
        }
        return [200, link];
      }
      if (method === 'DELETE') {
        links.delete(id);
        record(graph, link.item_id, actor, 'unlink', { ...link }, null);
        settleFrom(graph, actor, [link.item_id]);
        return [204];
      }
      return;
    }
    if (collection === 'events' && sub === 'undo' && method === 'POST') {
      const event = events.find((e) => e.id === id);
      if (!event) return;
      if (event.automatic) throw [409, "the rules made that change, from the item's links and parts; change those instead"];
      if (events.some((e) => e.undoes === id)) throw [409, 'that change has been undone already'];
      return [201, undo(event)];
    }
  }

  function undo(event) {
    const changed = () => [409, 'it has changed since; undo the later changes first'];
    const item = items.get(event.item_id);
    const graph = event.graph_id;
    let kind = event.kind;
    let [before, after] = [event.after, event.before];
    const touched = [];
    switch (event.kind) {
      case 'rename': {
        const g = workGraphs.find((x) => x.id === graph);
        if (g.name !== before.name) throw changed();
        g.name = after.name;
        break;
      }
      case 'create':
        if (item.state === 'dropped') throw [409, 'the item is dropped already'];
        kind = 'state';
        before = { state: item.state };
        after = { state: 'dropped' };
        setState(item.id, 'dropped', false);
        touched.push(item.id);
        break;
      case 'edit':
        if (!same({ title: item.title, description: item.description }, before)) throw changed();
        Object.assign(item, after);
        break;
      case 'state':
        if (item.state !== before.state) throw changed();
        setState(item.id, after.state, false);
        touched.push(item.id);
        break;
      case 'move':
        if (item.parent_id !== before.parent_id || item.rank !== before.rank) throw changed();
        touched.push(item.id, ...(item.parent_id ? [item.parent_id] : []));
        Object.assign(item, after);
        break;
      case 'block':
        if (!edges.some(([b, x]) => b === before.blocker && x === before.blocked)) throw changed();
        edges = edges.filter(([b, x]) => !(b === before.blocker && x === before.blocked));
        kind = 'unblock';
        break;
      case 'unblock':
        if (edges.some(([b, x]) => b === after.blocker && x === after.blocked)) throw changed();
        if (deadlocked([...edges, [after.blocker, after.blocked]])) throw [409, 'that would close a cycle'];
        edges.push([after.blocker, after.blocked]);
        kind = 'block';
        break;
      case 'link':
        if (!same(links.get(before.id), before)) throw changed();
        links.delete(before.id);
        kind = 'unlink';
        touched.push(event.item_id);
        break;
      case 'unlink':
        if (links.has(after.id)) throw [409, 'the item links that again already'];
        links.set(after.id, { ...after });
        kind = 'link';
        touched.push(event.item_id);
        break;
      case 'link_status': {
        const link = links.get(before.link);
        if (!link || link.status !== before.status) throw changed();
        link.status = after.status;
        touched.push(event.item_id);
        break;
      }
      default:
        throw [409, `a ${event.kind} change can't be undone`];
    }
    const inverse = record(graph, event.item_id, actor, kind, before, after, event.id);
    settleFrom(graph, actor, touched);
    return inverse;
  }
}
