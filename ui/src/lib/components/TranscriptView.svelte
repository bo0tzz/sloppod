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
    sessionId,
  }: { items: TranscriptItem[]; running?: boolean; sessionId?: string } = $props();

  // The turn each item is part of: the message that started it, steered messages aside.
  const turns = $derived.by(() => {
    let turn: string | null = null;
    return items.map((item) => {
      if (item.type === 'user' && !item.steering) turn = item.messageId;
      return turn;
    });
  });

  function bundleUrl(index: number): string | undefined {
    const turn = turns[index];
    if (!sessionId || !turn) return undefined;
    return `/api/v1/sessions/${encodeURIComponent(sessionId)}/debug-bundle?message=${encodeURIComponent(turn)}`;
  }
</script>

<div class="flex flex-col gap-3" data-testid="transcript">
  {#each items as item, index (item.key)}
    {#if item.type === 'user' && item.notice}
      <SystemNotice message={item} />
    {:else if item.type === 'user' || item.type === 'agent'}
      <MessageView message={item} bundleUrl={item.type === 'agent' ? bundleUrl(index) : undefined} />
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
