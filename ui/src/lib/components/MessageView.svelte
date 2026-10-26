<script lang="ts">
  import { Icon } from '@immich/ui';
  import { mdiLinkVariant } from '@mdi/js';
  import type { MessageItem } from '#lib/acp/transcript.svelte.ts';
  import Markdown from './Markdown.svelte';

  let { message }: { message: MessageItem } = $props();

  const user = $derived(message.type === 'user');
</script>

<div class="flex {user ? 'justify-end' : ''}" data-testid="message-{message.type}">
  <div class={user ? 'bg-primary/10 max-w-[85%] rounded-2xl rounded-ee-md px-4 py-2.5' : 'w-full min-w-0'}>
    {#each message.parts as part, index (index)}
      {#if part.type === 'text'}
        <Markdown text={part.text} breaks={user} />
      {:else if part.type === 'image'}
        <img src={part.src} alt="" class="my-2 max-h-80 rounded-lg" />
      {:else}
        <a
          href={part.uri}
          target="_blank"
          rel="noopener noreferrer"
          class="bg-light-100 my-1 inline-flex items-center gap-1 rounded-lg px-2 py-1 text-sm"
        >
          <Icon icon={mdiLinkVariant} aria-hidden />
          {part.name}
        </a>
      {/if}
    {/each}
  </div>
</div>
