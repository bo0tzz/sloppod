<script lang="ts">
  import type { TranscriptItem } from '#lib/acp/transcript.svelte.ts';
  import CompactionView from './CompactionView.svelte';
  import MessageView from './MessageView.svelte';
  import NoticeView from './NoticeView.svelte';
  import PlanView from './PlanView.svelte';
  import SystemNotice from './SystemNotice.svelte';
  import ThoughtView from './ThoughtView.svelte';
  import ToolCallView from './ToolCallView.svelte';

  let {
    items,
    running = false,
    unreadFrom = null,
  }: { items: TranscriptItem[]; running?: boolean; unreadFrom?: string | null } = $props();
</script>

<div class="flex flex-col gap-3" data-testid="transcript">
  {#each items as item, index (item.key)}
    {#if item.key === unreadFrom}
      <div class="text-primary flex items-center gap-2 text-xs font-medium" role="separator" data-testid="last-read">
        <div class="bg-primary/40 h-px grow"></div>
        New since you last read
        <div class="bg-primary/40 h-px grow"></div>
      </div>
    {/if}
    {#if item.type === 'user' && item.notice}
      <SystemNotice message={item} />
    {:else if item.type === 'user' || item.type === 'agent'}
      <MessageView message={item} />
    {:else if item.type === 'thought'}
      <ThoughtView thought={item} active={running && index === items.length - 1} />
    {:else if item.type === 'tool'}
      <ToolCallView tool={item} />
    {:else if item.type === 'plan'}
      <PlanView plan={item} />
    {:else if item.type === 'compaction'}
      <CompactionView compaction={item} />
    {:else if item.type === 'notice'}
      <NoticeView notice={item} />
    {/if}
  {/each}
</div>
