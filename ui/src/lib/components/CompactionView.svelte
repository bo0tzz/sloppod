<script lang="ts">
  import { Icon, LoadingSpinner } from '@immich/ui';
  import { mdiArrowCollapseVertical, mdiChevronRight } from '@mdi/js';
  import type { CompactionItem } from '#lib/acp/transcript.svelte.ts';
  import Markdown from './Markdown.svelte';

  let { compaction }: { compaction: CompactionItem } = $props();

  const label = $derived(
    {
      in_progress: 'Compacting context…',
      completed: 'Context compacted',
      failed: 'Context compaction failed',
      cancelled: 'Context compaction cancelled',
    }[compaction.status] ?? `Context compaction: ${compaction.status}`,
  );
  const expandable = $derived(Boolean(compaction.summary || compaction.error));
</script>

<details class="group" data-testid="compaction" data-status={compaction.status}>
  <summary
    class="text-muted flex list-none items-center gap-3 text-xs select-none [&::-webkit-details-marker]:hidden {expandable
      ? 'cursor-pointer'
      : 'pointer-events-none'}"
  >
    <span class="bg-light-300 h-px grow"></span>
    {#if compaction.status === 'in_progress'}
      <LoadingSpinner size="tiny" />
    {:else}
      <Icon icon={mdiArrowCollapseVertical} aria-hidden />
    {/if}
    <span class="font-medium">{label}</span>
    {#if expandable}
      <Icon icon={mdiChevronRight} class="transition-transform group-open:rotate-90" aria-hidden />
    {/if}
    <span class="bg-light-300 h-px grow"></span>
  </summary>
  <div class="mt-2 flex flex-col gap-2">
    {#if compaction.error}
      <p class="text-danger text-sm">{compaction.error}</p>
    {/if}
    {#if compaction.summary}
      <div class="bg-light-100 rounded-xl p-3 text-sm">
        <Markdown text={compaction.summary} />
      </div>
    {/if}
  </div>
</details>
