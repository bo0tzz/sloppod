<script lang="ts">
  import { Icon } from '@immich/ui';
  import { mdiChevronRight, mdiWrenchOutline } from '@mdi/js';
  import type { MessageItem, ToolItem } from '#lib/acp/transcript.svelte.ts';
  import { toolStatuses, toolTitle } from '#lib/presentation.ts';
  import ThoughtView from './ThoughtView.svelte';
  import ToolCallView from './ToolCallView.svelte';

  /**
   * A run of tool calls, and the thoughts between them, folded into one block (#446): "12 tool calls" and, while one
   * is running, its label. Expanded on click; its calls are only drawn then, so a long run costs nothing while it's
   * folded. What needs the user (a failed call, a highlight, a question or a permission request) isn't in a fold.
   */
  let { items }: { items: (ToolItem | MessageItem)[] } = $props();
  const calls = $derived(items.filter((item): item is ToolItem => item.type === 'tool'));

  let open = $state(false);
  const running = $derived(calls.findLast((call) => call.status === 'in_progress' || call.status === 'pending'));
  const status = $derived(toolStatuses[running ? running.status : 'completed']);
</script>

<details bind:open class="group/fold border-light-200 bg-light-50 rounded-xl border" data-testid="tool-fold" data-running={running ? 'true' : 'false'}>
  <summary class="flex cursor-pointer list-none items-center gap-2 px-3 py-2 text-sm [&::-webkit-details-marker]:hidden">
    <Icon icon={mdiWrenchOutline} class="text-muted shrink-0" size="1.1em" aria-hidden />
    <span class="shrink-0" data-testid="tool-fold-count">{calls.length} tool calls</span>
    {#if running}
      <span class="text-muted min-w-0 grow truncate text-xs" data-testid="tool-fold-running" title={running.title}>
        {toolTitle(running.title).title}
      </span>
    {:else}
      <span class="grow"></span>
    {/if}
    <Icon icon={status.icon} class="shrink-0 {status.class}" size="1.1em" title={status.label} />
    <Icon icon={mdiChevronRight} class="text-muted shrink-0 transition-transform group-open/fold:rotate-90" aria-hidden />
  </summary>
  {#if open}
    <div class="border-light-200 flex flex-col gap-2 border-t p-2">
      {#each items as item (item.key)}
        {#if item.type === 'tool'}
          <ToolCallView tool={item} />
        {:else}
          <ThoughtView thought={item} active={false} />
        {/if}
      {/each}
    </div>
  {/if}
</details>
