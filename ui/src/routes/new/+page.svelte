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
  /** What can be chosen at all: the capabilities suggested for an empty draft, whichever draft is current. */
  let offered = $state<string[]>([]);
  let suggested = $state(false);
  /** The suggestion shown was made for the draft as it is now. */
  let fresh = $state(false);
  let pending = $state(false);
  let source = $state<StartingSet['source'] | null>(null);
  let title = $state('');
  let repo = $state<string | null>(null);
  let capabilities = $state<string[]>([]);
  const touched = { title: false, repo: false, capabilities: false };
  /** Advances with every change to the draft, so an answer to an earlier one is dropped, debounce or not. */
  let generation = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;

  /** The suggestion shown, for the draft it was made for. */
  let shown: StartingSet | null = null;

  /** Asks for the suggestion for `prompt`, and shows it if the draft hasn't changed since. Resolves with it either
   * way, or null if it didn't come, so a caller keeps what it asked for whatever the card shows by then. */
  function suggest(prompt: string): Promise<StartingSet | null> {
    const mine = generation;
    pending = true;
    return api.suggestSession(prompt).then(
      (set) => {
        if (prompt === '') offered = set.capabilities;
        else if (offered.length === 0) loadOffered();
        if (mine === generation) {
          pending = false;
          fresh = true;
          suggested = true;
          shown = set;
          source = set.source;
          if (!touched.title) title = set.title;
          if (!touched.repo) repo = set.repo;
          if (!touched.capabilities) capabilities = set.capabilities;
        }
        return set;
      },
      () => {
        if (mine === generation) pending = false;
        return null;
      },
    );
  }

  /** Asks again what can be chosen, after the empty draft's answer didn't arrive. */
  function loadOffered() {
    api.suggestSession('').then(
      (set) => (offered = set.capabilities),
      () => {},
    );
  }

  onMount(() => {
    if (!matchMedia('(pointer: coarse)').matches) textarea?.focus();
    api.listRepos().then(
      (list) => (repos = list),
      () => {},
    );
    // Once with the empty draft, which also warms the runtime up for the first real one.
    void suggest('');
    return () => clearTimeout(timer);
  });

  let previous = '';
  $effect(() => {
    const prompt = text.trim();
    if (prompt === previous) return;
    previous = prompt;
    generation++;
    fresh = false;
    pending = true;
    clearTimeout(timer);
    timer = setTimeout(() => void suggest(prompt), SUGGEST_AFTER);
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
      // A suggestion for an earlier draft isn't sent for this one: the suggestion for it is waited for, so what the
      // user didn't change follows their message. If it doesn't come, only what they set themselves is sent, and
      // the session starts as it always did for the rest.
      // What's sent comes from the suggestion for this prompt, not from the card, which the user may have kept typing
      // into meanwhile and which then shows the suggestion for a later one.
      let set = fresh ? shown : null;
      if (!fresh) {
        clearTimeout(timer);
        set = await suggest(prompt);
      }
      const session = await api.createSession({
        prompt,
        title: touched.title ? title.trim() || undefined : set?.title,
        repo: touched.repo ? repo : set?.repo,
        capabilities: touched.capabilities ? capabilities : set?.capabilities,
      });
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
