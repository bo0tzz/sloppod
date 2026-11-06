<script lang="ts">
  import { Icon, Text } from '@immich/ui';
  import { mdiSourceBranch, mdiTimerSandComplete } from '@mdi/js';
  import type { Session } from '#lib/api.ts';
  import { absoluteTime, clockTime, timeAgo } from '#lib/format.ts';
  import PhaseBadge from './PhaseBadge.svelte';

  let {
    sessions,
    activeId,
    compact = false,
    unknown = false,
  }: { sessions: Session[]; activeId?: string; compact?: boolean; unknown?: boolean } = $props();

  // Sub-sessions are listed under their parent, and a tree counts as running while any of it is.
  const children = $derived.by(() => {
    const ids = new Set(sessions.map((session) => session.id));
    const byParent = new Map<string, Session[]>();
    for (const session of sessions) {
      if (session.parent_id && ids.has(session.parent_id)) {
        byParent.set(session.parent_id, [...(byParent.get(session.parent_id) ?? []), session]);
      }
    }
    return byParent;
  });
  const roots = $derived(sessions.filter((session) => !session.parent_id || !sessions.some((s) => s.id === session.parent_id)));
  const isRunning = (session: Session): boolean =>
    session.phase !== 'parked' || (children.get(session.id) ?? []).some(isRunning);
  // What the user does first: a session with something open for them (a question, a permission request, a proposal, a
  // highlight), then one at work, then the rest, the most recent first within each (#445). A tree goes by its most
  // urgent session.
  const rank = (session: Session): number => {
    const own =
      session.phase === 'waiting' ||
      Object.values(session.waiting ?? {}).some((count) => count > 0)
        ? 0
        : session.phase === 'working' || session.phase === 'starting'
          ? 1
          : 2;
    return Math.min(own, ...(children.get(session.id) ?? []).map(rank));
  };
  const running = $derived(roots.filter(isRunning).sort((a, b) => rank(a) - rank(b)));
  const groups = $derived(
    [
      { key: 'waiting', title: 'Waiting on you', sessions: running.filter((s) => rank(s) === 0) },
      { key: 'working', title: 'Working', sessions: running.filter((s) => rank(s) === 1) },
      { key: 'rest', title: 'Other', sessions: running.filter((s) => rank(s) === 2) },
    ].filter((group) => group.sessions.length > 0),
  );
  // Only told apart by a heading when there's more than one group; a long working group can be folded away.
  const FOLDABLE = 5;
  let workingFolded = $state(false);
  const parked = $derived(roots.filter((session) => !isRunning(session)));

  let container = $state<HTMLElement>();

  /** Arrow keys move between the rows, in the order they're shown, sub-sessions included. */
  function onkeydown(event: KeyboardEvent) {
    const step = { ArrowDown: 1, ArrowUp: -1 }[event.key];
    if (!step || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const rows = [...(container?.querySelectorAll<HTMLElement>('[data-testid="session-row"]') ?? [])];
    const index = rows.indexOf(event.target as HTMLElement);
    if (index === -1) return;
    event.preventDefault();
    rows[Math.min(Math.max(index + step, 0), rows.length - 1)].focus();
  }

  let now = $state(Date.now());
  $effect(() => {
    const timer = setInterval(() => (now = Date.now()), 30_000);
    return () => clearInterval(timer);
  });
</script>

{#snippet row(session: Session)}
  {@const active = session.id === activeId}
  <li>
    <a
      href="/sessions/{session.id}"
      aria-current={active ? 'page' : undefined}
      data-testid="session-row"
      class="flex flex-col gap-1 transition-colors {compact
        ? 'rounded-xl px-3 py-2'
        : 'border-light-200 bg-light-50 rounded-2xl border px-4 py-3'} {active
        ? 'bg-primary/10 text-primary'
        : 'hover:bg-light-100'}"
    >
      <div class="flex items-center gap-2">
        <span class="min-w-0 grow truncate font-medium {compact ? 'text-sm' : ''}">
          {session.title || 'New session'}
        </span>
        {#if session.limited_until}
          <span class="text-warning shrink-0" title="Waiting for the usage limit; carries on at {clockTime(session.limited_until, now)}" data-testid="session-limited">
            <Icon icon={mdiTimerSandComplete} size="1.1em" aria-label="Waiting for the usage limit" />
          </span>
        {/if}
        <PhaseBadge phase={session.phase} commands={session.commands} commandLines={session.command_lines} waiting={session.waiting} {unknown} />
      </div>
      <div class="text-muted flex min-w-0 items-center gap-1.5 text-xs">
        {#if session.repo}
          <Icon icon={mdiSourceBranch} size="1.1em" class="shrink-0" aria-hidden />
          <span class="truncate" title={session.branch ?? undefined}>{session.repo}</span>
          <span aria-hidden="true">·</span>
        {/if}
        <time class="shrink-0" datetime={session.active_at} title={absoluteTime(session.active_at)}>
          {timeAgo(session.active_at, now)}
        </time>
        {#if session.parent_id && session.model}
          <span aria-hidden="true">·</span>
          <span class="truncate" title="Runs on {session.model}" data-testid="session-model">{session.model}</span>
        {/if}
      </div>
    </a>
    {#if children.get(session.id)?.length}
      <ul class="border-light-200 mt-0.5 ml-4 flex flex-col border-l pl-2 {compact ? 'gap-0.5' : 'gap-2 pt-2'}" data-testid="sub-sessions">
        {#each children.get(session.id) ?? [] as child (child.id)}
          {@render row(child)}
        {/each}
      </ul>
    {/if}
  </li>
{/snippet}

<!-- The rows are links, focusable as they are; the handler only adds the arrow keys. -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div bind:this={container} {onkeydown} class="contents">
<ul class="flex flex-col {compact ? 'gap-0.5 p-2' : 'gap-2'}" data-testid="session-list">
  {#each groups as group (group.key)}
    {@const foldable = group.key === 'working' && group.sessions.length > FOLDABLE}
    <!-- The heading, and with it the control that unfolds, stays while a working group can be folded, other groups or not. -->
    {#if groups.length > 1 || foldable}
      <li class="flex items-center justify-between {compact ? 'px-3 pt-1' : 'pt-1'}" data-testid="session-group" data-group={group.key}>
        <Text size="tiny" color="muted" fontWeight="semi-bold">{group.title}{group.key === 'working' ? ` (${group.sessions.length})` : ''}</Text>
        {#if foldable}
          <button type="button" class="text-muted text-xs underline" aria-expanded={!workingFolded} onclick={() => (workingFolded = !workingFolded)}>
            {workingFolded ? 'Show' : 'Fold'}
          </button>
        {/if}
      </li>
    {/if}
    {#if !(foldable && workingFolded)}
      {#each group.sessions as session (session.id)}
        {@render row(session)}
      {/each}
    {/if}
  {:else}
    <li>
      <Text color="muted" size="small" class="px-3 py-6 text-center">{parked.length ? 'No running sessions.' : 'No sessions yet.'}</Text>
    </li>
  {/each}
</ul>
{#if parked.length > 0}
  <section class="flex flex-col {compact ? 'px-2 pb-2' : 'mt-6 gap-2'}" data-testid="parked-sessions">
    <Text size="tiny" color="muted" fontWeight="semi-bold" class={compact ? 'px-3 py-1' : ''}>Parked</Text>
    <ul class="flex flex-col {compact ? 'gap-0.5' : 'gap-2'}">
      {#each parked as session (session.id)}
        {@render row(session)}
      {/each}
    </ul>
  </section>
{/if}
</div>
