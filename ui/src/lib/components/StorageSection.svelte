<script lang="ts">
  import { Alert, Button, Heading, LoadingSpinner, Text, modalManager } from '@immich/ui';
  import { mdiTrashCanOutline } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api, type Session, type StoredVolume } from '#lib/api.ts';
  import { bytes } from '#lib/format.ts';
  import { sessions } from '#lib/sessions.svelte.ts';
  import PhaseBadge from './PhaseBadge.svelte';

  /**
   * Every volume the cluster holds for the user's sessions, of any kind and deleted ones included, and those whose
   * session nobody knows (#455): the most used first, then those never measured.
   */
  let volumes = $state<StoredVolume[] | null>(null);
  let loadError = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  let busy = $state<string | null>(null);

  const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));
  const used = (volume: StoredVolume) => volume.session?.disk?.used;
  const known = $derived((volumes ?? []).filter((volume) => volume.session));
  const unknown = $derived((volumes ?? []).filter((volume) => !volume.session));
  const measured = $derived(known.filter((volume) => used(volume) !== undefined));
  const total = $derived(measured.reduce((sum, volume) => sum + (used(volume) ?? 0), 0));
  const unmeasured = $derived((volumes ?? []).length - measured.length);

  async function refresh() {
    try {
      const storage = await api.getStorage();
      volumes = storage.volumes.sort((a, b) => (used(b) ?? -1) - (used(a) ?? -1));
      loadError = null;
    } catch (e) {
      loadError = errorText(e);
    }
  }

  onMount(() => {
    void refresh();
  });

  async function act(id: string, action: () => Promise<Session | void>) {
    busy = id;
    actionError = null;
    try {
      const session = await action();
      if (session) sessions.upsert(session);
      await refresh();
    } catch (e) {
      actionError = errorText(e);
    } finally {
      busy = null;
    }
  }

  async function remove(session: Session) {
    const confirmed = await modalManager.showDialog({
      title: `Delete ${session.title || 'this session'}?`,
      prompt: 'It can be restored for 7 days; then its volume and everything else of it go.',
      confirmText: 'Delete',
      icon: mdiTrashCanOutline,
    });
    if (confirmed) await act(session.id, () => api.deleteSession(session.id));
  }

  const count = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`;
</script>

<section class="flex flex-col gap-3" data-testid="storage" aria-labelledby="storage-title">
  <div class="flex flex-col gap-1">
    <Heading tag="h2" size="tiny" id="storage-title">Storage</Heading>
    <Text color="muted" size="small">
      Each session keeps its files on a volume of its own while it's parked. A stopped session's volume goes a week after
      it stopped, unless you keep it; a deleted one's when it can't be restored any more. A session says how much it uses
      while it runs, and once more as it parks. A volume that's filling up grows on its own, before it runs out of room.
    </Text>
  </div>

  {#if loadError}
    <Alert color="danger" size="small" title="Couldn't load the sessions' storage">{loadError}</Alert>
  {:else if volumes === null}
    <div class="flex py-4"><LoadingSpinner /></div>
  {:else if volumes.length === 0}
    <Text color="muted" size="small">No volumes.</Text>
  {:else}
    <Text size="small" data-testid="storage-total">
      {count(volumes.length, 'volume', 'volumes')}, {bytes(total)} used{#if unmeasured > 0}; {unmeasured} not measured{/if}
    </Text>
    {#if known.length > 0}
      <ul class="border-light-200 divide-light-200 divide-y rounded-xl border text-sm">
        {#each known as volume (volume.name)}
          {@const session = volume.session!}
          <li class="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2" data-testid="storage-session" data-session={session.id}>
            <a href="/sessions/{session.id}" class="min-w-0 grow truncate font-medium hover:underline">{session.title || 'New session'}</a>
            {#if session.kind === 'review'}
              <Text color="muted" size="small" data-testid="storage-reviewer">reviewer</Text>
            {/if}
            {#if session.deleted_at}
              <Text color="muted" size="small">deleted</Text>
            {:else}
              <PhaseBadge phase={session.phase} />
            {/if}
            <span class="text-muted w-24 shrink-0 text-end" data-testid="storage-used">
              {#if session.disk}
                {bytes(session.disk.used)}
              {:else}
                not measured
              {/if}
            </span>
            {#if volume.capacity !== null}
              <!-- The volume's size as the cluster has it now (#479): what it's been grown to, not what the session last reported. -->
              <Text color="muted" size="small" data-testid="storage-size">of {bytes(volume.capacity)}</Text>
            {/if}
            {#if !session.deleted_at && session.kind === 'work'}
              <div class="flex shrink-0 gap-1">
                {#if session.phase === 'stopped'}
                  <Button
                    size="tiny"
                    shape="round"
                    variant="ghost"
                    disabled={busy !== null}
                    onclick={() => act(session.id, () => api.setVolumeKept(session.id, !session.volume_kept))}
                  >
                    {session.volume_kept ? 'Let it go' : 'Keep volume'}
                  </Button>
                {:else if session.phase !== 'parked'}
                  <Button
                    size="tiny"
                    shape="round"
                    variant="ghost"
                    disabled={busy !== null}
                    aria-label="Park the session {session.title || 'New session'}"
                    onclick={() => act(session.id, () => api.parkSession(session.id))}
                  >
                    Park
                  </Button>
                {/if}
                <Button
                  size="tiny"
                  shape="round"
                  variant="ghost"
                  color="danger"
                  disabled={busy !== null}
                  aria-label="Delete the session {session.title || 'New session'}"
                  onclick={() => remove(session)}
                >
                  Delete
                </Button>
              </div>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
    {#if unknown.length > 0}
      <div class="flex flex-col gap-2" data-testid="storage-unknown">
        <Text size="small" fontWeight="semi-bold">{count(unknown.length, 'volume', 'volumes')} of no known session</Text>
        <Text color="muted" size="small">
          Left alone: sloppod doesn't remove what its database doesn't know, in case that's the database's mistake. See
          docs/deploy.md for removing them by hand.
        </Text>
        <ul class="border-light-200 divide-light-200 divide-y rounded-xl border text-sm">
          {#each unknown as volume (volume.name)}
            <li class="px-3 py-2 font-mono text-xs break-all" data-testid="storage-unknown-volume">{volume.name}</li>
          {/each}
        </ul>
      </div>
    {/if}
  {/if}
  {#if actionError}
    <Alert color="danger" size="small">{actionError}</Alert>
  {/if}
</section>
