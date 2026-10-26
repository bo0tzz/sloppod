<script lang="ts">
  import { Icon } from '@immich/ui';
  import { mdiChevronRight, mdiLightbulbOutline } from '@mdi/js';
  import type { MessageItem } from '#lib/acp/transcript.svelte.ts';
  import Markdown from './Markdown.svelte';

  let { thought, active = false }: { thought: MessageItem; active?: boolean } = $props();

  const text = $derived(thought.parts.map((part) => (part.type === 'text' ? part.text : '')).join(''));
  const preview = $derived(text.trim().split('\n')[0]);
</script>

<details class="group text-muted" data-testid="thought">
  <summary class="flex cursor-pointer list-none items-center gap-1.5 text-sm select-none [&::-webkit-details-marker]:hidden">
    <Icon icon={mdiChevronRight} class="shrink-0 transition-transform group-open:rotate-90" aria-hidden />
    <Icon icon={mdiLightbulbOutline} class="shrink-0 {active ? 'animate-pulse' : ''}" aria-hidden />
    <span class="shrink-0 font-medium">{active ? 'Thinking…' : 'Thought'}</span>
    <span class="min-w-0 truncate italic opacity-80 group-open:hidden">{preview}</span>
  </summary>
  <div class="border-light-300 mt-2 ms-2 border-s-2 ps-4 text-sm">
    <Markdown {text} />
  </div>
</details>
