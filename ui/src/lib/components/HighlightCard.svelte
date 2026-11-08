<script lang="ts">
  import { Button, Icon } from '@immich/ui';
  import { mdiCheck, mdiStarFourPointsOutline } from '@mdi/js';
  import type { Highlight } from '#lib/api.ts';
  import { absoluteTime, timeAgo, timeWithDay } from '#lib/format.ts';
  import { isSnoozed } from '#lib/snooze.ts';
  import { userTimezone } from '#lib/user-timezone.svelte.ts';
  import Markdown from './Markdown.svelte';
  import SnoozeButton from './SnoozeButton.svelte';

  /**
   * What the agent highlighted for the user: in the chat, where its tool call was, and in the session's "for you" list.
   * Open until the user marks it seen, which is theirs alone: no turn starts and the agent isn't told. They can put one
   * off until later instead (#467), when the runtime brings it back.
   */
  let {
    highlight,
    onSeen,
    onSnooze,
    testid = 'highlight-card',
  }: {
    highlight: Highlight;
    onSeen: (highlight: Highlight) => Promise<void>;
    onSnooze?: (highlight: Highlight, until: Date) => Promise<void>;
    testid?: string;
  } = $props();

  const retracted = $derived(Boolean(highlight.retracted_at));
  const open = $derived(!highlight.seen_at && !retracted);
  const snoozed = $derived(isSnoozed(highlight));
  let busy = $state(false);
  let error = $state<string | null>(null);

  async function run(what: string, action: () => Promise<void>) {
    busy = true;
    error = null;
    try {
      await action();
    } catch (cause) {
      // Not saved: the buttons stay, to try again.
      error = `Couldn't ${what}: ${cause instanceof Error ? cause.message : String(cause)}`;
    } finally {
      busy = false;
    }
  }
</script>

<div
  class="flex flex-col gap-2 rounded-2xl border px-4 py-3 text-left {open
    ? 'border-primary/40 bg-primary/5'
    : 'border-light-200 bg-light-50 text-muted'}"
  data-testid={testid}
  data-seen={highlight.seen_at ? 'true' : 'false'}
  data-snoozed={snoozed ? 'true' : 'false'}
  data-retracted={retracted ? 'true' : 'false'}
>
  <div class="flex items-center gap-2 text-xs {open ? 'text-primary' : 'text-muted'}">
    <Icon icon={mdiStarFourPointsOutline} size="1.1em" aria-hidden />
    <span class="font-medium">{retracted ? 'Retracted' : snoozed ? 'Snoozed' : open ? 'For you' : 'Seen'}</span>
    {#if snoozed}
      {@const until = timeWithDay(highlight.snoozed_until!, userTimezone.value)}
      <time
        class="text-muted"
        datetime={highlight.snoozed_until}
        title="Snoozed until {until.title}"
        data-testid="highlight-snoozed-until"
      >
        until {until.label}
      </time>
    {:else}
      <time class="text-muted" datetime={highlight.created_at} title={absoluteTime(highlight.created_at)}>
        {timeAgo(highlight.created_at)}
      </time>
    {/if}
  </div>
  <Markdown text={highlight.text} class="text-sm {retracted ? 'line-through' : ''}" />
  {#if retracted}
    <p class="text-sm" data-testid="highlight-retraction">Retracted: {highlight.retraction_reason}</p>
  {/if}
  {#if open}
    <div class="flex items-center gap-2">
      <Button size="small" shape="round" leadingIcon={mdiCheck} loading={busy} onclick={() => run('mark it seen', () => onSeen(highlight))}>
        Seen
      </Button>
      {#if onSnooze}
        <SnoozeButton loading={busy} onSnooze={(until) => run('snooze it', () => onSnooze(highlight, until))} />
      {/if}
    </div>
    {#if error}
      <p class="text-danger mt-1 text-xs" data-testid="highlight-error">{error}</p>
    {/if}
  {/if}
</div>
