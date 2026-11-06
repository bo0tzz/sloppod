<script lang="ts">
  import { Icon } from '@immich/ui';
  import { mdiOpenInNew, mdiSourceBranch, mdiSourcePull } from '@mdi/js';
  import type { WorkDone } from '#lib/api.ts';
  import { timeAgo } from '#lib/format.ts';

  let { items }: { items: WorkDone[] } = $props();

  const stateLabel = { open: 'Open', merged: 'Merged', closed: 'Closed' } as const;
  const stateColor = { open: 'text-success-700', merged: 'text-primary', closed: 'text-muted' } as const;
</script>

{#if items.length === 0}
  <p class="text-muted text-sm" data-testid="work-done-empty">Nothing yet: the issues and pull requests the session acts on will be listed here.</p>
{:else}
  <ul class="flex flex-col gap-2" data-testid="work-done">
    {#each items as item (`${item.repo}#${item.number}`)}
      <li class="flex flex-col gap-0.5 text-sm" data-testid="work-done-item">
        <a
          href={item.url}
          target="_blank"
          rel="noreferrer"
          class="group flex min-w-0 items-center gap-1.5 hover:underline"
          title={item.title ?? `${item.repo}#${item.number}`}
        >
          <Icon icon={item.kind === 'pull_request' ? mdiSourcePull : mdiSourceBranch} size="1.1em" class="text-muted shrink-0" aria-hidden />
          <span class="text-muted shrink-0 font-mono text-xs">{item.repo}#{item.number}</span>
          <Icon icon={mdiOpenInNew} size="0.9em" class="text-muted ms-auto shrink-0 opacity-0 group-hover:opacity-100 group-focus-visible:opacity-100" aria-hidden />
        </a>
        {#if item.title}
          <span class="truncate ps-6 font-medium" data-testid="work-done-title">{item.title}</span>
        {/if}
        <span class="text-muted flex flex-wrap gap-x-2 ps-6 text-xs">
          <span data-testid="work-done-did">{item.did.join(', ')}</span>
          <time datetime={item.at}>{timeAgo(item.at)}</time>
          {#if item.state}
            <span class={stateColor[item.state.state]} data-testid="work-done-state">{stateLabel[item.state.state]}</span>
            {#if item.state.state === 'open' && (item.state.checks === 'failure' || item.state.checks === 'error')}
              <span class="text-danger-700" data-testid="work-done-checks">
                checks failing{item.state.failing?.length ? `: ${item.state.failing.join(', ')}` : ''}
              </span>
            {:else if item.state.state === 'open' && item.state.checks === 'success'}
              <span class="text-success-700" data-testid="work-done-checks">checks passing</span>
            {/if}
          {/if}
        </span>
      </li>
    {/each}
  </ul>
{/if}
