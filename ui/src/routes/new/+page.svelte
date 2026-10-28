<script lang="ts">
  import { goto } from '$app/navigation';
  import { ContextMenuButton, Text } from '@immich/ui';
  import { mdiCogOutline, mdiSend } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api, ApiError, type StartingSet } from '#lib/api.ts';
  import ChatHeader from '#lib/components/ChatHeader.svelte';
  import Composer from '#lib/components/Composer.svelte';
  import StartingCard from '#lib/components/StartingCard.svelte';
  import { sessions } from '#lib/sessions.svelte.ts';

  let text = $state('');
  let sending = $state(false);
  let error = $state<{ message: string; noAccount: boolean } | null>(null);
  let repos = $state<string[]>([]);
  let textarea = $state<HTMLTextAreaElement | null>(null);

  // The starting set (#232): suggested while the message is written, shown as a card the user edits. A field they
  // changed keeps their value when a later suggestion arrives; the others follow it.
  const SUGGEST_AFTER = 800;
  let offered = $state<string[]>([]);
  let suggested = $state(false);
  let pending = $state(false);
  let source = $state<StartingSet['source'] | null>(null);
  let title = $state('');
  let repo = $state<string | null>(null);
  let capabilities = $state<string[]>([]);
  const touched = { title: false, repo: false, capabilities: false };
  let asked = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;

  function suggest(prompt: string) {
    const mine = ++asked;
    pending = true;
    api.suggestSession(prompt).then(
      (set) => {
        if (mine !== asked) return;
        pending = false;
        // The first answer, for an empty draft, says what can be chosen at all.
        if (!suggested) offered = set.capabilities;
        suggested = true;
        source = set.source;
        if (!touched.title) title = set.title;
        if (!touched.repo) repo = set.repo;
        if (!touched.capabilities) capabilities = set.capabilities;
      },
      () => {
        if (mine === asked) pending = false;
      },
    );
  }

  onMount(() => {
    if (!matchMedia('(pointer: coarse)').matches) textarea?.focus();
    api.listRepos().then(
      (list) => (repos = list),
      () => {},
    );
    // Once with the empty draft, which also warms the runtime up for the first real one.
    suggest('');
    return () => clearTimeout(timer);
  });

  let previous = '';
  $effect(() => {
    const prompt = text.trim();
    if (prompt === previous) return;
    previous = prompt;
    clearTimeout(timer);
    timer = setTimeout(() => suggest(prompt), SUGGEST_AFTER);
  });

  function toggle(capability: string, on: boolean) {
    touched.capabilities = true;
    capabilities = on ? [...new Set([...capabilities, capability])] : capabilities.filter((c) => c !== capability);
  }

  async function send() {
    const prompt = text.trim();
    if (!prompt || sending) return;
    sending = true;
    error = null;
    try {
      // Without a suggestion yet, the session starts as it always did.
      const session = await api.createSession(
        suggested ? { prompt, title: title.trim() || undefined, repo, capabilities } : { prompt },
      );
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
      {#if suggested}
        <div class="mt-6 w-full">
          <StartingCard
            {title}
            {repo}
            {capabilities}
            {offered}
            {repos}
            {pending}
            {source}
            onTitle={(value) => {
              touched.title = true;
              title = value;
            }}
            onRepo={(value) => {
              touched.repo = true;
              repo = value;
            }}
            onCapability={toggle}
          />
        </div>
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
