<script lang="ts">
  import { Icon } from '@immich/ui';
  import { mdiLinkVariant, mdiSteering } from '@mdi/js';
  import type { MessageItem } from '#lib/acp/transcript.svelte.ts';
  import Markdown from './Markdown.svelte';

  let { message }: { message: MessageItem } = $props();

  const user = $derived(message.type === 'user');
</script>

<div class="flex {user ? 'justify-end' : ''}" data-testid="message-{message.type}" data-steering={message.steering || undefined}>
  <div
    class={user
      ? `max-w-[85%] rounded-2xl rounded-ee-md px-4 py-2.5 ${message.steering ? 'bg-warning-500/15' : 'bg-primary/10'}`
      : 'w-full min-w-0'}
  >
    {#if message.steering}
      <div class="text-warning-700 dark:text-warning-400 mb-1 flex items-center gap-1 text-xs font-medium">
        <Icon icon={mdiSteering} aria-hidden />
        Steered into the running turn
      </div>
    {/if}
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
