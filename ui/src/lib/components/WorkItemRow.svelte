<script lang="ts">
  import { Badge } from '@immich/ui';
  import type { WorkItem } from '#lib/api.ts';
  import { linkHref, linkLabel, stateColor } from '#lib/work.ts';

  /** One item in a list: its title (which opens it), its state, what blocks it and what it links to. */
  let {
    item,
    titleOf,
    depth = 0,
    onOpen,
  }: { item: WorkItem; titleOf: (id: string) => string; depth?: number; onOpen: (item: WorkItem) => void } = $props();
</script>

<div class="flex min-w-0 flex-col gap-1 py-1" style:padding-inline-start="{depth * 1.25}rem" data-testid="work-item" data-title={item.title}>
  <div class="flex min-w-0 items-center gap-2">
    <button type="button" class="min-w-0 truncate text-start font-medium hover:underline" onclick={() => onOpen(item)}>
      {item.title}
    </button>
    <Badge color={stateColor[item.state]} size="tiny" shape="round" class="shrink-0" data-testid="work-state">{item.state}</Badge>
  </div>
  {#if item.blocked_by.length > 0 || item.links.length > 0}
    <div class="text-muted flex flex-wrap gap-x-3 gap-y-1 text-xs">
      {#if item.blocked_by.length > 0}
        <span data-testid="blocked-by">Blocked by {item.blocked_by.map(titleOf).join(', ')}</span>
      {/if}
      {#each item.links as link (link.id)}
        {@const href = linkHref(link)}
        {#if href}
          <a class="underline" {href} target={href.startsWith('http') ? '_blank' : undefined} rel="noreferrer">{linkLabel(link)}</a>
        {:else}
          <span>{linkLabel(link)}</span>
        {/if}
      {/each}
    </div>
  {/if}
</div>
