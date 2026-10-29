import type { WorkEvent, WorkItem, WorkLink, WorkState } from '#lib/api.ts';

/** The states in the order a graph's page groups them, after its ready list. */
export const states: { state: WorkState; label: string }[] = [
  { state: 'active', label: 'Active' },
  { state: 'open', label: 'Open, not ready' },
  { state: 'done', label: 'Done' },
  { state: 'dropped', label: 'Dropped' },
];

export const stateColor: Record<WorkState, 'primary' | 'secondary' | 'success' | 'info'> = {
  open: 'info',
  active: 'primary',
  done: 'success',
  dropped: 'secondary',
};

export const linkKinds = [
  { value: 'issue', label: 'Issue' },
  { value: 'pull_request', label: 'Pull request' },
  { value: 'session', label: 'Session' },
  { value: 'proposal', label: 'Proposal' },
] as const;

/** Where a link's target lives: on GitHub for issues and pull requests, its page for a session; none for a proposal. */
export function linkHref(link: Pick<WorkLink, 'kind' | 'target'>): string | null {
  const github = /^([^/#]+\/[^/#]+)#(\d+)$/.exec(link.target);
  if (github && link.kind === 'issue') return `https://github.com/${github[1]}/issues/${github[2]}`;
  if (github && link.kind === 'pull_request') return `https://github.com/${github[1]}/pull/${github[2]}`;
  if (link.kind === 'session') return `/sessions/${encodeURIComponent(link.target)}`;
  return null;
}

export function linkLabel(link: Pick<WorkLink, 'kind' | 'target'>): string {
  const kind = linkKinds.find((k) => k.value === link.kind)?.label ?? link.kind;
  const short = link.kind === 'session' || link.kind === 'proposal' ? link.target.slice(0, 8) : link.target;
  return `${kind} ${short}`;
}

/** The items of a graph as a tree: each item's parts follow it, by rank, with their depth. */
export function tree(items: WorkItem[]): { item: WorkItem; depth: number }[] {
  const byParent = new Map<string | null, WorkItem[]>();
  for (const item of items) {
    const parent = item.parent_id && items.some((i) => i.id === item.parent_id) ? item.parent_id : null;
    byParent.set(parent, [...(byParent.get(parent) ?? []), item]);
  }
  const out: { item: WorkItem; depth: number }[] = [];
  const walk = (parent: string | null, depth: number) => {
    for (const item of (byParent.get(parent) ?? []).sort((a, b) => a.rank - b.rank)) {
      out.push({ item, depth });
      walk(item.id, depth + 1);
    }
  };
  walk(null, 0);
  return out;
}

/** The siblings of an item, by rank: the items with the same parent. */
export function siblings(items: WorkItem[], item: WorkItem): WorkItem[] {
  return items.filter((i) => i.parent_id === item.parent_id).sort((a, b) => a.rank - b.rank);
}

/** What a change in an item's history did, in a line. */
export function describeEvent(event: WorkEvent, titleOf: (id: unknown) => string): string {
  const before = event.before ?? {};
  const after = event.after ?? {};
  switch (event.kind) {
    case 'create':
      return `created it`;
    case 'edit':
      return before.title !== after.title ? `renamed it from “${before.title}” to “${after.title}”` : 'edited its description';
    case 'state':
      return `${before.state} → ${after.state}`;
    case 'move':
      return before.parent_id !== after.parent_id
        ? `moved it under ${after.parent_id ? titleOf(after.parent_id) : 'the top level'}`
        : 'reranked it';
    case 'block':
      return `${titleOf(after.blocker)} blocks ${titleOf(after.blocked)}`;
    case 'unblock':
      return `${titleOf(before.blocker)} no longer blocks ${titleOf(before.blocked)}`;
    case 'link':
      return `linked ${linkLabel(after as Pick<WorkLink, 'kind' | 'target'>)}`;
    case 'unlink':
      return `unlinked ${linkLabel(before as Pick<WorkLink, 'kind' | 'target'>)}`;
    case 'link_status':
      return `a link went ${before.status} → ${after.status}`;
    case 'rename':
      return `renamed the graph to ${after.name}`;
    default:
      return event.kind;
  }
}

/**
 * What an item waits on now: the unfinished items that block it or an item it's part of (blockers are inherited), as
 * the runtime's frontier counts them. Each says which item it blocks when that's an ancestor.
 */
export function waitingOn(item: WorkItem, items: WorkItem[]): { blocker: WorkItem; through: WorkItem | null }[] {
  const byId = new Map(items.map((i) => [i.id, i]));
  const waiting: { blocker: WorkItem; through: WorkItem | null }[] = [];
  const seen = new Set<string>();
  for (let at: WorkItem | undefined = item; at && !seen.has(at.id); at = at.parent_id ? byId.get(at.parent_id) : undefined) {
    seen.add(at.id);
    for (const id of at.blocked_by) {
      const blocker = byId.get(id);
      if (blocker && blocker.state !== 'done' && blocker.state !== 'dropped') {
        waiting.push({ blocker, through: at.id === item.id ? null : at });
      }
    }
  }
  return waiting;
}
