<script lang="ts">
  import { Icon, Text } from '@immich/ui';
  import { mdiSourceBranch } from '@mdi/js';
  import type { Session } from '#lib/api.ts';
  import { absoluteTime, timeAgo } from '#lib/format.ts';
  import PhaseBadge from './PhaseBadge.svelte';

  let {
    sessions,
    activeId,
    compact = false,
  }: { sessions: Session[]; activeId?: string; compact?: boolean } = $props();

  const running = $derived(sessions.filter((session) => session.phase !== 'parked'));
  const parked = $derived(sessions.filter((session) => session.phase === 'parked'));

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
        <PhaseBadge phase={session.phase} />
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
      </div>
    </a>
  </li>
{/snippet}

<ul class="flex flex-col {compact ? 'gap-0.5 p-2' : 'gap-2'}" data-testid="session-list">
  {#each running as session (session.id)}
    {@render row(session)}
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
