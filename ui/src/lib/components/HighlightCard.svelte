<script lang="ts">
  import { Button, Icon } from '@immich/ui';
  import { mdiCheck, mdiStarFourPointsOutline } from '@mdi/js';
  import type { Highlight } from '#lib/api.ts';
  import { absoluteTime, timeAgo } from '#lib/format.ts';
  import Markdown from './Markdown.svelte';

  /**
   * What the agent highlighted for the user: in the chat, where its tool call was, and in the session's "for you" list.
   * Open until the user marks it seen, which is theirs alone: no turn starts and the agent isn't told.
   */
  let {
    highlight,
    onSeen,
    testid = 'highlight-card',
  }: { highlight: Highlight; onSeen: (highlight: Highlight) => Promise<void>; testid?: string } = $props();

  const retracted = $derived(Boolean(highlight.retracted_at));
  const open = $derived(!highlight.seen_at && !retracted);
  let busy = $state(false);
  let error = $state<string | null>(null);

  async function seen() {
    busy = true;
    error = null;
    try {
      await onSeen(highlight);
    } catch (cause) {
      // Not saved: Seen stays, to try again.
      error = cause instanceof Error ? cause.message : String(cause);
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
  data-retracted={retracted ? 'true' : 'false'}
>
  <div class="flex items-center gap-2 text-xs {open ? 'text-primary' : 'text-muted'}">
    <Icon icon={mdiStarFourPointsOutline} size="1.1em" aria-hidden />
    <span class="font-medium">{retracted ? 'Retracted' : open ? 'For you' : 'Seen'}</span>
    <time class="text-muted" datetime={highlight.created_at} title={absoluteTime(highlight.created_at)}>
      {timeAgo(highlight.created_at)}
    </time>
  </div>
  <Markdown text={highlight.text} class="text-sm {retracted ? 'line-through' : ''}" />
  {#if retracted}
    <p class="text-sm" data-testid="highlight-retraction">Retracted: {highlight.retraction_reason}</p>
  {/if}
  {#if open}
    <div>
      <Button size="small" shape="round" leadingIcon={mdiCheck} loading={busy} onclick={seen}>Seen</Button>
      {#if error}
        <p class="text-danger mt-1 text-xs" data-testid="highlight-error">Couldn't mark it seen: {error}</p>
      {/if}
    </div>
  {/if}
</div>
