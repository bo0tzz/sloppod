<script lang="ts">
  import { goto } from '$app/navigation';
  import { ContextMenuButton, Text } from '@immich/ui';
  import { mdiCogOutline, mdiSend } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api, ApiError } from '#lib/api.ts';
  import ChatHeader from '#lib/components/ChatHeader.svelte';
  import Composer from '#lib/components/Composer.svelte';
  import { sessions } from '#lib/sessions.svelte.ts';

  let text = $state('');
  let sending = $state(false);
  let error = $state<{ message: string; noAccount: boolean } | null>(null);
  let repos = $state<string[]>([]);
  let textarea = $state<HTMLTextAreaElement | null>(null);

  onMount(() => {
    if (!matchMedia('(pointer: coarse)').matches) textarea?.focus();
    api.listRepos().then(
      (list) => (repos = list),
      () => {},
    );
  });

  async function send() {
    const prompt = text.trim();
    if (!prompt || sending) return;
    sending = true;
    error = null;
    try {
      const session = await api.createSession({ prompt });
      sessions.upsert(session);
      await goto(`/sessions/${session.id}`);
    } catch (e) {
      error = {
        message: e instanceof Error ? e.message : String(e),
        noAccount: e instanceof ApiError && e.status === 409,
      };
      sending = false;
    }
  }
</script>

<svelte:head>
  <title>New session · sloppod</title>
</svelte:head>

<div class="flex h-full min-h-0 flex-col" data-testid="new-session">
  <ChatHeader title="New session">
    {#snippet actions()}
      <ContextMenuButton aria-label="More" items={[{ title: 'Settings', icon: mdiCogOutline, href: '/settings' }]} />
    {/snippet}
  </ChatHeader>

  <div class="min-h-0 grow overflow-y-auto">
    <div class="mx-auto flex w-full max-w-3xl flex-col items-center gap-2 px-4 py-16 text-center">
      <Text color="muted" size="large">What do you want to work on?</Text>
      {#if repos.length > 0}
        <Text color="muted" size="small">
          Mention a repository to check it out, e.g. <span class="font-mono">{repos[0]}</span>.
        </Text>
      {/if}
    </div>
  </div>

  <Composer
    bind:text
    bind:ref={textarea}
    label="Prompt"
    placeholder="Message the agent…"
    sendLabel="Send"
    sendIcon={mdiSend}
    canSend={text.trim().length > 0 && !sending}
    {sending}
    onSend={send}
    notice={error ? notice : undefined}
  />
</div>

{#snippet notice()}
  {#if error?.noAccount}
    <span class="text-danger" data-testid="composer-error">
      No Claude account is configured. <a href="/settings" class="underline">Add one in settings</a> to start a session.
    </span>
  {:else if error}
    <span class="text-danger" data-testid="composer-error">Couldn't start the session: {error.message}</span>
  {/if}
{/snippet}
