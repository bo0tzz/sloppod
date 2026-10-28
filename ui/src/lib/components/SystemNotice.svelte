<script lang="ts">
  import { Icon } from '@immich/ui';
  import { mdiBellOutline, mdiChevronDown } from '@mdi/js';
  import type { MessageItem } from '#lib/acp/transcript.svelte.ts';
  import Markdown from './Markdown.svelte';

  let { message }: { message: MessageItem } = $props();

  const text = $derived(message.parts.map((part) => (part.type === 'text' ? part.text : '')).join(''));
</script>

{#if message.summary}
  <details class="text-muted group px-1 text-sm" data-testid="system-notice">
    <summary class="flex cursor-pointer list-none items-center gap-2 [&::-webkit-details-marker]:hidden">
      <Icon icon={mdiBellOutline} class="shrink-0" aria-label="From sloppod" />
      <span class="min-w-0 grow truncate">{message.summary}</span>
      <Icon icon={mdiChevronDown} class="shrink-0 transition-transform group-open:rotate-180" aria-hidden />
    </summary>
    <Markdown {text} breaks class="mt-1 ms-6 min-w-0" />
  </details>
{:else}
  <div class="text-muted flex items-start gap-2 px-1 text-sm" data-testid="system-notice">
    <Icon icon={mdiBellOutline} class="mt-1 shrink-0" aria-label="From sloppod" />
    <Markdown {text} breaks class="min-w-0 grow" />
  </div>
{/if}
