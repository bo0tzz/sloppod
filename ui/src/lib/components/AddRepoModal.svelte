<script lang="ts">
  import { Alert, Button, Field, LoadingSpinner, Modal, ModalBody, ModalFooter, Select, Text } from '@immich/ui';
  import { mdiSourceRepository } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api, type Session } from '#lib/api.ts';

  let {
    session,
    onAdded,
    onClose,
  }: { session: Session; onAdded: (session: Session, repo: string) => void; onClose: () => void } = $props();

  let repos = $state<string[] | null>(null);
  let repo = $state<string>();
  let saving = $state(false);
  let error = $state<string | null>(null);

  const granted = $derived(
    new Set(session.grants.filter((grant) => grant.capability === 'repo').map((grant) => String(grant.params.repo))),
  );
  const available = $derived((repos ?? []).filter((candidate) => !granted.has(candidate)));

  onMount(async () => {
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
      onAdded(await api.addSessionRepo(session.id, repo), repo);
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
      {#if repos === null}
        <div class="flex justify-center py-4"><LoadingSpinner /></div>
      {:else if available.length === 0 && !error}
        <Text size="small">
          No more repositories available. <a href="/settings" class="text-primary underline">Check your GitHub App in settings</a>.
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
