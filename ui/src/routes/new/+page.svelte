<script lang="ts">
  import { goto } from '$app/navigation';
  import { Text } from '@immich/ui';
  import MenuButton from '#lib/components/MenuButton.svelte';
  import { mdiCogOutline, mdiSend } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api, ApiError, type GitHubUserConfig, type StartingGrant, type StartingSet } from '#lib/api.ts';
  import ChatHeader from '#lib/components/ChatHeader.svelte';
  import Composer from '#lib/components/Composer.svelte';
  import StartingCard, { type OwnCapability, type RepoRow, type TokenChoice } from '#lib/components/StartingCard.svelte';
  import { sessions } from '#lib/sessions.svelte.ts';

  let text = $state('');
  let sending = $state(false);
  let error = $state<{ message: string; noAccount: boolean } | null>(null);
  let repos = $state<string[]>([]);
  let textarea = $state<HTMLTextAreaElement | null>(null);

  // The starting set (#232): suggested while the message is written, shown as a card the user edits. A field they
  // changed keeps their value when a later suggestion arrives; the others follow it.
  const SUGGEST_AFTER = 800;
  /** How long sending waits for the suggestion for the message being sent. */
  const SEND_WAITS_AT_MOST = 3000;
  /** What can be chosen at all: the capabilities suggested for an empty draft, whichever draft is current. */
  let offered = $state<string[]>([]);
  let suggested = $state(false);
  /** The suggestion shown was made for the draft as it is now. */
  let fresh = $state(false);
  let pending = $state(false);
  let source = $state<StartingSet['source'] | null>(null);
  let title = $state('');
  // The starting set is a list of grants: repositories, each with who it acts as, and the capabilities chosen. A
  // suggestion fills in the repository and the built-ins; the user's own capabilities and the identity a repository
  // acts as are only ever theirs to choose.
  let rows = $state<RepoRow[]>([]);
  let capabilities = $state<string[]>([]);
  let own = $state<OwnCapability[]>([]);
  let ownChosen = $state<string[]>([]);
  let tokens = $state<TokenChoice[]>([]);
  let nextRow = 0;
  const touched = { title: false, repos: false, capabilities: false };
  const suggestedRows = (repo: string | null): RepoRow[] => (repo ? [{ id: nextRow++, repo, credential: null }] : []);
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
          if (!touched.repos) rows = suggestedRows(set.repo);
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
    // What the card can add beyond the suggestion; without them it offers the App's repositories and built-ins alone.
    api.listCapabilities().then(
      (list) => (own = list.map((definition) => ({ name: definition.name, description: definition.description }))),
      () => {},
    );
    api.listCredentials().then(
      (list) =>
        (tokens = list
          .filter((credential) => credential.kind === 'github-user')
          .map((credential) => {
            const config = credential.config as unknown as GitHubUserConfig;
            return {
              id: credential.id,
              label: `${config.login} (${credential.name})`,
              repositories: config.repositories,
              more: config.more_repositories,
            };
          })),
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

  const toggleOwn = (name: string, on: boolean) =>
    (ownChosen = on ? [...new Set([...ownChosen, name])] : ownChosen.filter((chosen) => chosen !== name));

  function changeRow(id: number, change: Partial<RepoRow>) {
    touched.repos = true;
    rows = rows.map((row) => (row.id === id ? { ...row, ...change } : row));
  }

  /** The grants a starting set comes to: its repositories, then the built-ins and own capabilities chosen. */
  function startingGrants(repoRows: RepoRow[], builtIns: string[]): StartingGrant[] {
    return [
      ...repoRows
        .filter((row) => row.repo)
        .map((row): StartingGrant => ({
          capability: 'repo',
          params: { repo: row.repo },
          ...(row.credential ? { credential: row.credential } : {}),
        })),
      ...builtIns.map((capability) => ({ capability })),
      ...ownChosen.map((capability) => ({ capability })),
    ];
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
      // It's waited for briefly: a judgement can take a minute when the runtime's utility session is cold, and
      // starting the session shouldn't. Past that, the runtime derives what the user didn't set, as before.
      let set = fresh ? shown : null;
      if (!fresh) {
        clearTimeout(timer);
        set = await Promise.race([
          suggest(prompt),
          new Promise<null>((resolve) => setTimeout(() => resolve(null), SEND_WAITS_AT_MOST)),
        ]);
      }
      // What the user set is sent as they set it, the rest from the suggestion for this prompt. With nothing set and no
      // suggestion, no grants are sent and the runtime derives the starting set as it always did.
      const anything = set || touched.repos || touched.capabilities || ownChosen.length > 0;
      const repoRows = touched.repos || !set ? rows : suggestedRows(set.repo);
      const builtIns = touched.capabilities || !set ? capabilities : set.capabilities;
      const session = await api.createSession({
        prompt,
        title: touched.title ? title.trim() || undefined : set?.title,
        grants: anything ? startingGrants(repoRows, builtIns) : undefined,
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
      <MenuButton aria-label="More" items={[{ title: 'Settings', icon: mdiCogOutline, href: '/settings' }]} />
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
            {rows}
            {capabilities}
            {offered}
            {own}
            {ownChosen}
            {tokens}
            {repos}
            {pending}
            {source}
            onTitle={(value) => {
              touched.title = true;
              title = value;
            }}
            onAddRepo={() => {
              touched.repos = true;
              rows = [...rows, { id: nextRow++, repo: '', credential: null }];
            }}
            onRemoveRepo={(id) => {
              touched.repos = true;
              rows = rows.filter((row) => row.id !== id);
            }}
            onRepo={(id, repo) => changeRow(id, { repo })}
            onIdentity={(id, credential) => changeRow(id, { credential, repo: '' })}
            onCapability={toggle}
            onOwn={toggleOwn}
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
