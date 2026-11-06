<script lang="ts">
  import { Alert, Button, Heading, LoadingSpinner, Text, modalManager } from '@immich/ui';
  import { mdiTrashCanOutline } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api, type Session } from '#lib/api.ts';
  import { bytes } from '#lib/format.ts';
  import { sessions } from '#lib/sessions.svelte.ts';
  import PhaseBadge from './PhaseBadge.svelte';

  /** Sessions with a workspace volume, deleted ones included until they're purged, largest first (#455). */
  let listed = $state<Session[] | null>(null);
  let loadError = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  let busy = $state<string | null>(null);

  const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));
  const used = (session: Session) => session.disk?.used ?? 0;
  const total = $derived((listed ?? []).reduce((sum, session) => sum + used(session), 0));

  async function refresh() {
    try {
      const [live, deleted] = await Promise.all([api.listSessions(), api.listDeletedSessions()]);
      listed = [...live, ...deleted].filter((session) => session.disk).sort((a, b) => used(b) - used(a));
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
</script>

<section class="flex flex-col gap-3" data-testid="storage" aria-labelledby="storage-title">
  <div class="flex flex-col gap-1">
    <Heading tag="h2" size="tiny" id="storage-title">Storage</Heading>
    <Text color="muted" size="small">
      Each session keeps its files on a volume of its own while it's parked. A stopped session's volume goes a week after
      it stopped, unless you keep it; a deleted one's when it can't be restored any more.
    </Text>
  </div>

  {#if loadError}
    <Alert color="danger" size="small" title="Couldn't load the sessions' storage">{loadError}</Alert>
  {:else if listed === null}
    <div class="flex py-4"><LoadingSpinner /></div>
  {:else if listed.length === 0}
    <Text color="muted" size="small">No session has said how much it uses yet.</Text>
  {:else}
    <Text size="small" data-testid="storage-total">{bytes(total)} used by {listed.length} {listed.length === 1 ? 'session' : 'sessions'}</Text>
    <ul class="border-light-200 divide-light-200 divide-y rounded-xl border text-sm">
      {#each listed as session (session.id)}
        <li class="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2" data-testid="storage-session">
          <a href="/sessions/{session.id}" class="min-w-0 grow truncate font-medium hover:underline">{session.title || 'New session'}</a>
          {#if session.deleted_at}
            <Text color="muted" size="small">deleted</Text>
          {:else}
            <PhaseBadge phase={session.phase} />
          {/if}
          <span class="text-muted w-16 shrink-0 text-end" title="of {bytes(session.disk?.size ?? 0)}" data-testid="storage-used">{bytes(used(session))}</span>
          {#if !session.deleted_at}
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
                <Button size="tiny" shape="round" variant="ghost" disabled={busy !== null} onclick={() => act(session.id, () => api.parkSession(session.id))}>
                  Park
                </Button>
              {/if}
              <Button size="tiny" shape="round" variant="ghost" color="danger" disabled={busy !== null} onclick={() => remove(session)}>
                Delete
              </Button>
            </div>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
  {#if actionError}
    <Alert color="danger" size="small">{actionError}</Alert>
  {/if}
</section>
