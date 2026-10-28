<script lang="ts">
  import { Alert, Button, Field, LoadingSpinner, Modal, ModalBody, ModalFooter, Select, Text } from '@immich/ui';
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
  let tokens = $state<{ id: string; label: string; repositories: string[] }[]>([]);
  let actAs = $state(APP);
  let repo = $state<string>();
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
            return { id: credential.id, label: `${config.login} (${credential.name})`, repositories: config.repositories };
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
    if (!repo || saving) return;
    saving = true;
    error = null;
    try {
      onAdded(await api.addSessionRepo(session.id, repo, token?.id), repo);
      onClose();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      saving = false;
    }
  }
</script>

<Modal title="Add repository" icon={mdiSourceRepository} size="small" {onClose}>
  <ModalBody>
    <div class="flex flex-col gap-3 pb-4" data-testid="add-repo">
      <Text color="muted" size="small">The session gets write access to its own branch in the repository.</Text>
      {#if tokens.length > 0}
        <Field label="Act as">
          <Select options={identities} bind:value={actAs} onChange={() => (repo = undefined)} />
        </Field>
        {#if token}
          <Text color="muted" size="small">What the session pushes there is attributed to that person.</Text>
        {/if}
      {/if}
      {#if repos === null}
        <div class="flex justify-center py-4"><LoadingSpinner /></div>
      {:else if available.length === 0 && !error}
        <Text size="small">
          No more repositories available.
          <a href="/settings" class="text-primary underline">Check your {token ? 'GitHub account' : 'GitHub App'} in settings</a>.
        </Text>
      {:else if available.length > 0}
        <Field label="Repository">
          <Select options={available} bind:value={repo} placeholder="Choose a repository" />
        </Field>
      {/if}
      {#if error}
        <Alert color="danger" size="small">{error}</Alert>
      {/if}
    </div>
  </ModalBody>
  <ModalFooter>
    <div class="flex w-full justify-end gap-2">
      <Button color="secondary" variant="ghost" shape="round" onclick={onClose}>Cancel</Button>
      <Button shape="round" disabled={!repo} loading={saving} onclick={add}>Add</Button>
    </div>
  </ModalFooter>
</Modal>
