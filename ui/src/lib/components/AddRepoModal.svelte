<script lang="ts">
  import { Alert, Button, Field, Input, LoadingSpinner, ModalBody, ModalFooter, Select, Text } from '@immich/ui';
  import Dialog from './Dialog.svelte';
  import { mdiSourceRepository } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api, type GitHubUserConfig, type Session } from '#lib/api.ts';

  let {
    session,
    onAdded,
    onClose,
  }: { session: Session; onAdded: (session: Session, repo: string) => void; onClose: () => void } = $props();

  const APP = 'app';

  let repos = $state<string[] | null>(null);
  let tokens = $state<{ id: string; label: string; repositories: string[]; more: boolean }[]>([]);
  let actAs = $state(APP);
  let repo = $state<string>();
  let typed = $state('');
  let saving = $state(false);
  let error = $state<string | null>(null);

  const token = $derived(tokens.find((candidate) => candidate.id === actAs));
  const identities = $derived([
    { value: APP, label: 'The GitHub App' },
    ...tokens.map((entry) => ({ value: entry.id, label: entry.label })),
  ]);
  // A repository the session has as the App can be switched to a token, but a token's can't go back to the App: adding
  // it without a credential would leave the grant acting as the person.
  const granted = $derived(
    new Set(
      session.grants
        .filter((grant) => grant.capability === 'repo' && (!token || grant.params.credential === token.id))
        .map((grant) => String(grant.params.repo)),
    ),
  );
  // A token that reaches more repositories than were listed when it was added can be given one that isn't listed.
  const unlisted = $derived(token?.more ?? false);
  const wanted = $derived(unlisted && typed.trim() ? typed.trim() : repo);
  const available = $derived((token ? token.repositories : (repos ?? [])).filter((candidate) => !granted.has(candidate)));

  onMount(async () => {
    // Without tokens, or if they can't be listed, the modal offers the App alone.
    api
      .listCredentials()
      .then((credentials) => {
        tokens = credentials
          .filter((credential) => credential.kind === 'github-user')
          .map((credential) => {
            const config = credential.config as unknown as GitHubUserConfig;
            return {
              id: credential.id,
              label: `${config.login} (${credential.name})`,
              repositories: config.repositories,
              more: config.more_repositories,
            };
          });
      })
      .catch(() => {});
    try {
      repos = await api.listRepos();
    } catch (e) {
      repos = [];
      error = e instanceof Error ? e.message : String(e);
    }
  });

  async function add() {
    if (!wanted || saving) return;
    saving = true;
    error = null;
    try {
      onAdded(await api.addSessionRepo(session.id, wanted, token?.id), wanted);
      onClose();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      saving = false;
    }
  }
</script>

<Dialog title="Add repository" icon={mdiSourceRepository} size="small" {onClose}>
  <ModalBody>
    <div class="flex flex-col gap-3 pb-4" data-testid="add-repo">
      <Text color="muted" size="small">The session gets write access to its own branch in the repository.</Text>
      {#if tokens.length > 0}
        <Field label="Act as">
          <Select options={identities} bind:value={actAs} onChange={() => {
              repo = undefined;
              typed = '';
            }} />
        </Field>
        {#if token}
          <Text color="muted" size="small">What the session pushes there is attributed to that person.</Text>
        {/if}
      {/if}
      {#if repos === null}
        <div class="flex justify-center py-4"><LoadingSpinner /></div>
      {:else if available.length === 0 && !error && !unlisted}
        <Text size="small">
          No more repositories available.
          <a href="/settings" class="text-primary underline">Check your {token ? 'GitHub account' : 'GitHub App'} in settings</a>.
        </Text>
      {:else if available.length > 0}
        <Field label="Repository">
          <Select options={available} bind:value={repo} placeholder="Choose a repository" />
        </Field>
      {/if}
      {#if unlisted}
        <Field label={available.length > 0 ? 'Or another repository' : 'Repository'}>
          <Input bind:value={typed} placeholder="owner/name" autocomplete="off" data-testid="unlisted-repo" />
        </Field>
        <Text color="muted" size="small">This token reaches more repositories than are listed. GitHub decides whether it reaches this one.</Text>
      {/if}
      {#if error}
        <Alert color="danger" size="small">{error}</Alert>
      {/if}
    </div>
  </ModalBody>
  <ModalFooter>
    <div class="flex w-full justify-end gap-2">
      <Button color="secondary" variant="ghost" shape="round" onclick={onClose}>Cancel</Button>
      <Button shape="round" disabled={!wanted} loading={saving} onclick={add}>Add</Button>
    </div>
  </ModalFooter>
</Dialog>
