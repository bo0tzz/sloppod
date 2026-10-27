<script lang="ts">
  import { Icon } from '@immich/ui';
  import { mdiBellOutline } from '@mdi/js';
  import type { MessageItem } from '#lib/acp/transcript.svelte.ts';
  import Markdown from './Markdown.svelte';

  let { message }: { message: MessageItem } = $props();

  const text = $derived(message.parts.map((part) => (part.type === 'text' ? part.text : '')).join(''));
</script>

<div class="text-muted flex items-start gap-2 px-1 text-sm" data-testid="system-notice">
  <Icon icon={mdiBellOutline} class="mt-1 shrink-0" aria-label="From sloppod" />
  <Markdown {text} breaks class="min-w-0 grow" />
</div>
