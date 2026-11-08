<script lang="ts">
  import type { MessageItem, ToolItem, TranscriptItem } from '#lib/acp/transcript.svelte.ts';
  import type { Highlight } from '#lib/api.ts';
  import { matchHighlights } from '#lib/highlights.ts';
  import CompactionView from './CompactionView.svelte';
  import HighlightCard from './HighlightCard.svelte';
  import MessageView from './MessageView.svelte';
  import NoticeView from './NoticeView.svelte';
  import PlanView from './PlanView.svelte';
  import SystemNotice from './SystemNotice.svelte';
  import ThoughtView from './ThoughtView.svelte';
  import ToolCallView from './ToolCallView.svelte';
  import ToolFold from './ToolFold.svelte';

  let {
    items,
    running = false,
    sessionId,
    highlights = [],
    onSeen,
    onSnooze,
    onResume,
    awaiting = new Set<string>(),
    unreadAfter = null,
  }: {
    items: TranscriptItem[];
    running?: boolean;
    sessionId?: string;
    /** What the agent highlighted: a `highlight` tool call that made one shows as its card. */
    highlights?: Highlight[];
    onSeen?: (highlight: Highlight) => Promise<void>;
    /** Puts a highlight off until later (#467). */
    onSnooze?: (highlight: Highlight, until: Date) => Promise<void>;
    onResume?: () => void;
    /** The ids of the tool calls waiting for the user's permission: they stay out of a fold. */
    awaiting?: Set<string>;
    /** The seq of the last event read (#48): an "Unread" bar goes above the first item after it. `null` for no bar. */
    unreadAfter?: number | null;
  } = $props();
  const highlighted = $derived(matchHighlights(items, highlights));

  /** The sloppod tools that put something in front of the user: a question, a proposal, a report. */
  const ASKS = /(^|__|\.)(ask_user|AskUserQuestion|propose_[a-z_]+|request_repository|request_owner_identity|report_done)$/;
  /** What stays out of a fold because the user needs to see it: a highlight, a failed call, a permission request... */
  const needsUser = (item: ToolItem) =>
    highlighted.has(item.toolCallId) ||
    item.status === 'failed' ||
    awaiting.has(item.toolCallId) ||
    ASKS.test(item.title) ||
    /(^|__|\.)(highlight|retract_highlight)$/.test(item.title);

  /**
   * What's drawn: each item as it is, except that a run with two or more tool calls folds into one block (#446). A run is
   * tool calls with nothing in them the user needs, and the thoughts between them, which agents interleave with their
   * calls; the fold goes from its first call to its last, so a thought before or after it, such as the live one at the
   * end of a running turn, stays out in the open. It keeps the place of its first call.
   */
  type Folded = ToolItem | MessageItem;
  type Row = { item: TranscriptItem; index: number } | { fold: Folded[]; key: string; index: number };
  const rows = $derived.by((): Row[] => {
    const rows: Row[] = [];
    let run: { item: Folded; index: number }[] = [];
    const flush = () => {
      const calls = run.flatMap((entry, at) => (entry.item.type === 'tool' ? [at] : []));
      if (calls.length >= 2) {
        const first = calls[0];
        const last = calls[calls.length - 1];
        rows.push(...run.slice(0, first));
        const folded = run.slice(first, last + 1);
        rows.push({ fold: folded.map((entry) => entry.item), key: `fold-${folded[0].item.key}`, index: folded[0].index });
        rows.push(...run.slice(last + 1));
      } else {
        rows.push(...run);
      }
      run = [];
    };
    items.forEach((item, index) => {
      if ((item.type === 'tool' && !needsUser(item)) || item.type === 'thought') {
        run.push({ item, index });
      } else {
        flush();
        rows.push({ item, index });
      }
    });
    flush();
    return rows;
  });

  const rowItems = (row: Row) => ('fold' in row ? row.fold : [row.item]);
  /**
   * What a row is, where it has a name the rows of an older page don't share: a message's id or a tool call's. A page
   * going in above the chat leaves it alone, and the updates a live turn adds to a row don't change it, so a row can be
   * found again by it once the chat's history has grown above it.
   */
  const rowName = (row: Row) => {
    const first = rowItems(row)[0];
    if (first.type === 'user' || first.type === 'agent') return first.messageId ? `message:${first.messageId}` : '';
    if (first.type === 'tool') return `tool:${first.toolCallId}`;
    return '';
  };
  /** The last event that touched the row, the first one that made an item in it, and the last that added to one: the bar goes above the first row with something added after the read position. */
  const rowSeq = (row: Row) => Math.max(...rowItems(row).map((item) => item.seq));
  const rowFirstSeq = (row: Row) => Math.min(...rowItems(row).map((item) => item.firstSeq));
  const rowAddedSeq = (row: Row) => Math.max(...rowItems(row).map((item) => item.addedSeq));
  const firstUnread = $derived(
    unreadAfter === null ? -1 : rows.findIndex((row) => rowAddedSeq(row) > unreadAfter),
  );

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
  {#each rows as row, at ('fold' in row ? row.key : row.item.key)}
    {#if at === firstUnread}
      <div class="text-primary flex items-center gap-3 text-xs font-medium" data-testid="unread-divider" data-unread-divider>
        <span class="bg-primary/40 h-px grow"></span>
        Unread
        <span class="bg-primary/40 h-px grow"></span>
      </div>
    {/if}
    <!-- Off-screen rows aren't laid out or painted until they come near (a long transcript stays smooth). -->
    <div
      class="[contain-intrinsic-size:auto_4rem] [content-visibility:auto]"
      data-seq={rowSeq(row)}
      data-first-seq={rowFirstSeq(row)}
      data-row-name={rowName(row) || undefined}
    >
      {#if 'fold' in row}
        <ToolFold items={row.fold} />
      {:else}
        {@const item = row.item}
        {@const index = row.index}
        {#if item.type === 'user' && item.notice}
          <SystemNotice message={item} />
        {:else if item.type === 'user' || item.type === 'agent'}
          <MessageView message={item} bundleUrl={item.type === 'agent' ? bundleUrl(index) : undefined} />
        {:else if item.type === 'thought'}
          <ThoughtView thought={item} active={running && index === items.length - 1} />
        {:else if item.type === 'tool' && highlighted.has(item.toolCallId) && onSeen}
          <HighlightCard highlight={highlighted.get(item.toolCallId)!} {onSeen} {onSnooze} />
        {:else if item.type === 'tool'}
          <ToolCallView tool={item} />
        {:else if item.type === 'plan'}
          <PlanView plan={item} />
        {:else if item.type === 'compaction'}
          <CompactionView compaction={item} />
        {:else if item.type === 'notice'}
          <!-- Resume only from the last thing in the transcript: anything sent since has carried on already. -->
          <NoticeView notice={item} onResume={item.resume && !running && index === items.length - 1 ? onResume : undefined} />
        {/if}
      {/if}
    </div>
  {/each}
</div>
