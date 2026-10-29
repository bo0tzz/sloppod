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

  // The turn each item is part of, as the debug bundle's query names it: the message that started it, steered
  // messages aside. Loaded history can begin part way through a turn: what comes before the first message loaded is
  // in the turn before that message's or, with no message loaded, in the latest.
  const turns = $derived.by(() => {
    const starts = (item: TranscriptItem) =>
      item.type === 'user' && !item.steering && item.messageId ? item.messageId : null;
    const first = items.map(starts).find((id) => id !== null);
    let turn = first ? `?before=${encodeURIComponent(first)}` : '';
    return items.map((item) => {
      const id = starts(item);
      if (id) turn = `?message=${encodeURIComponent(id)}`;
      return turn;
    });
  });

  function bundleUrl(index: number): string | undefined {
    if (!sessionId) return undefined;
    return `/api/v1/sessions/${encodeURIComponent(sessionId)}/debug-bundle${turns[index]}`;
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
