<script lang="ts">
  import { Icon, IconButton } from '@immich/ui';
  import { mdiAlertCircleOutline, mdiCheck, mdiContentCopy, mdiLinkVariant, mdiSteering } from '@mdi/js';
  import { messageMarkdown, type MessageItem } from '#lib/acp/transcript.svelte.ts';
  import Markdown from './Markdown.svelte';

  let { message }: { message: MessageItem } = $props();

  const user = $derived(message.type === 'user');

  let copyState = $state<'idle' | 'copied' | 'failed'>('idle');
  let copyTimer: ReturnType<typeof setTimeout> | undefined;
  const copyFeedback = {
    idle: { icon: mdiContentCopy, label: 'Copy message', status: '' },
    copied: { icon: mdiCheck, label: 'Copied', status: 'Copied the message' },
    // The clipboard needs a secure context and the user's permission.
    failed: { icon: mdiAlertCircleOutline, label: "Couldn't copy", status: "Couldn't copy the message" },
  };

  async function copy() {
    try {
      await navigator.clipboard.writeText(messageMarkdown(message));
      copyState = 'copied';
    } catch {
      copyState = 'failed';
    }
    clearTimeout(copyTimer);
    copyTimer = setTimeout(() => (copyState = 'idle'), 2000);
  }

  $effect(() => () => clearTimeout(copyTimer));
</script>

<div class="flex {user ? 'justify-end' : ''}" data-testid="message-{message.type}" data-steering={message.steering || undefined}>
  <div
    class={user
      ? `max-w-[85%] rounded-2xl rounded-ee-md px-4 py-2.5 ${message.steering ? 'bg-warning-500/15' : 'bg-primary/10'}`
      : 'group w-full min-w-0'}
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
    {#if !user}
      <!-- Revealed on hover where there's a pointer to hover with, always shown on touch screens. -->
      <div data-testid="message-actions" class="pointer-fine:opacity-0 pointer-fine:group-hover:opacity-100 pointer-fine:focus-within:opacity-100 -ms-2 transition-opacity">
        <IconButton
          icon={copyFeedback[copyState].icon}
          aria-label={copyFeedback[copyState].label}
          variant="ghost"
          shape="round"
          color={copyState === 'failed' ? 'danger' : 'secondary'}
          size="small"
          onclick={copy}
        />
        <span class="sr-only" role="status">{copyFeedback[copyState].status}</span>
      </div>
    {/if}
  </div>
</div>
