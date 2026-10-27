<script lang="ts">
  import { Alert, Button, Heading, LoadingSpinner, Text } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api, type Session } from '#lib/api.ts';
  import { absoluteTime } from '#lib/format.ts';
  import { sessions } from '#lib/sessions.svelte.ts';

  /** How long a deleted session can be restored; the runtime purges it after that. */
  const RESTORABLE_DAYS = 7;

  let deleted = $state<Session[] | null>(null);
  let loadError = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  let restoring = $state<string | null>(null);

  const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));
  const purgedAt = (session: Session) =>
    new Date(Date.parse(session.deleted_at ?? '') + RESTORABLE_DAYS * 24 * 3600 * 1000).toISOString();

  async function refresh() {
    try {
      deleted = await api.listDeletedSessions();
      loadError = null;
    } catch (e) {
      loadError = errorText(e);
    }
  }

  onMount(() => {
    void refresh();
  });

  async function restore(id: string) {
    restoring = id;
    actionError = null;
    try {
      sessions.upsert(await api.restoreSession(id));
      await refresh();
    } catch (e) {
      actionError = errorText(e);
    } finally {
      restoring = null;
    }
  }
</script>

<section class="flex flex-col gap-3" data-testid="deleted-sessions" aria-labelledby="deleted-sessions-title">
  <div class="flex flex-col gap-1">
    <Heading tag="h2" size="tiny" id="deleted-sessions-title">Recently deleted</Heading>
    <Text color="muted" size="small">
      Deleted sessions can be restored for {RESTORABLE_DAYS} days, parked, with their files, branch and memory.
    </Text>
  </div>

  {#if loadError}
    <Alert color="danger" size="small" title="Couldn't load deleted sessions">{loadError}</Alert>
  {:else if deleted === null}
    <div class="flex py-4"><LoadingSpinner /></div>
  {:else if deleted.length === 0}
    <Text color="muted" size="small">None.</Text>
  {:else}
    <ul class="border-light-200 divide-light-200 divide-y rounded-xl border text-sm">
      {#each deleted as session (session.id)}
        <li class="flex items-center gap-3 px-3 py-2" data-testid="deleted-session">
          <span class="min-w-0 grow">
            <span class="font-medium">{session.title || 'New session'}</span>
            <span class="text-muted">, gone for good {absoluteTime(purgedAt(session))}</span>
          </span>
          <Button size="small" shape="round" variant="outline" loading={restoring === session.id} onclick={() => restore(session.id)}>
            Restore
          </Button>
        </li>
      {/each}
    </ul>
  {/if}
  {#if actionError}
    <Alert color="danger" size="small">{actionError}</Alert>
  {/if}
</section>
