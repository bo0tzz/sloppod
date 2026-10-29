<script lang="ts">
  import { Alert, Button, Field, ModalBody, Select, Text } from '@immich/ui';
  import Dialog from './Dialog.svelte';
  import { mdiShieldKeyOutline } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api, type Grant, type GrantChange, type Session, type WorkGraph, type WorkLevel } from '#lib/api.ts';
  import { clockTime } from '#lib/format.ts';

  let {
    session,
    onChange,
    onClose,
  }: { session: Session; onChange: (session: Session) => void; onClose: () => void } = $props();

  /** What can be granted while a session runs; repositories are added on their own. */
  const builtIn = ['model', 'github', 'packages', 'docs', 'codex', 'work'];
  /** The user's own capabilities, offered by name after the built-ins; without them, only the built-ins. */
  let own = $state<string[]>([]);
  /** The user's work graphs (ADR 30), for a `work` grant: one per graph. */
  let graphs = $state<WorkGraph[]>([]);
  onMount(() => {
    api.listCapabilities().then(
      (definitions) => (own = definitions.map((definition) => definition.name)),
      () => {},
    );
    api.listWorkGraphs().then(
      (listed) => (graphs = listed),
      () => {},
    );
  });
  const ungranted = $derived(
    graphs.filter((g) => !session.grants.some((grant) => grant.capability === 'work' && grant.params.graph === g.id)),
  );
  const levels = [
    { value: 'read', label: 'Read' },
    { value: 'track', label: 'Track' },
    { value: 'plan', label: 'Plan' },
  ];
  let graph = $state<string>();
  let level = $state<WorkLevel>('track');
  const grantable = $derived([...builtIn, ...own]);
  const durations: Record<string, number> = { hour: 3600_000, day: 24 * 3600_000, week: 7 * 24 * 3600_000 };
  const runsOut = [
    { value: 'never', label: 'Never' },
    { value: 'hour', label: 'In an hour' },
    { value: 'day', label: 'In a day' },
    { value: 'week', label: 'In a week' },
  ];
  const expiry = (choice: string) => (choice in durations ? new Date(Date.now() + durations[choice]).toISOString() : null);

  const changeable = $derived(session.phase !== 'stopped' && session.kind === 'work');
  const missing = $derived(
    grantable.filter((capability) =>
      capability === 'work' ? ungranted.length > 0 : !session.grants.some((grant) => grant.capability === capability),
    ),
  );
  let capability = $state<string>();
  let lasts = $state('never');
  let busy = $state(false);
  /** Bumped after every change, so the expiry pickers, which only act, show no choice again. */
  let settled = $state(0);
  let error = $state<string | null>(null);

  const narrowable = (grant: Grant) => grant.capability === 'repo' && grant.params.access === 'rw';

  async function act(call: () => Promise<Session>): Promise<boolean> {
    if (busy) return false;
    busy = true;
    error = null;
    try {
      onChange(await call());
      return true;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      return false;
    } finally {
      busy = false;
      settled++;
    }
  }

  const change = (grant: Grant, change: GrantChange) => act(() => api.changeGrant(session.id, grant.id!, change));
  const revoke = (grant: Grant) => act(() => api.revokeGrant(session.id, grant.id!));
  async function add() {
    const chosen = capability;
    if (!chosen || (chosen === 'work' && !graph)) return;
    const params = chosen === 'work' && graph ? { graph, level } : undefined;
    if (await act(() => api.grantSession(session.id, chosen, expiry(lasts) ?? undefined, params))) {
      capability = undefined;
      graph = undefined;
    }
  }
</script>

<Dialog title="Grants" icon={mdiShieldKeyOutline} size="medium" {onClose}>
  <ModalBody>
    <div class="flex flex-col gap-3 pb-4" data-testid="grants">
      <Text color="muted" size="small">What this session can reach and change. Anything else is refused.</Text>
      <ul class="flex flex-col gap-3">
        {#each session.grants as grant, index (grant.id ?? index)}
          <li class="flex flex-col gap-1" data-testid="grant-{grant.capability}">
            <span class="font-mono text-sm font-medium">{grant.capability}</span>
            <span class="text-sm">{grant.description ?? ''}</span>
            {#if grant.expires_at}
              <Text color="muted" size="small">Runs out {clockTime(grant.expires_at)}</Text>
            {/if}
            {#if changeable && grant.id}
              <div class="flex flex-wrap items-center gap-2">
                <div class="w-52">
                  {#key settled}
                    <Field disabled={busy}>
                      <Select
                        size="tiny"
                        options={runsOut}
                        placeholder="Change when it runs out"
                        title="When {grant.capability} runs out"
                        onChange={(value) => void change(grant, { expires_at: expiry(value as string) })}
                      />
                    </Field>
                  {/key}
                </div>
                <!-- Together, so Revoke never wraps onto a line of its own (#374). -->
                <div class="flex shrink-0 items-center gap-1">
                  {#if narrowable(grant)}
                    <Button size="tiny" variant="ghost" shape="round" disabled={busy} onclick={() => change(grant, { narrow: true })}>
                      Make read only
                    </Button>
                  {/if}
                  <Button size="tiny" variant="ghost" color="danger" shape="round" disabled={busy} onclick={() => revoke(grant)}>
                    Revoke
                  </Button>
                </div>
              </div>
            {/if}
          </li>
        {/each}
      </ul>
      {#if changeable && missing.length > 0}
        <div class="flex flex-wrap items-end gap-2 border-t pt-3" data-testid="add-grant">
          <div class="w-40">
            <Field label="Grant">
              <Select options={missing} bind:value={capability} placeholder="Capability" />
            </Field>
          </div>
          {#if capability === 'work'}
            <div class="w-40">
              <Field label="Graph">
                <Select options={ungranted.map((g) => ({ value: g.id, label: g.name }))} bind:value={graph} placeholder="Work graph" />
              </Field>
            </div>
            <div class="w-32">
              <Field label="Level">
                <Select options={levels} bind:value={level} />
              </Field>
            </div>
          {/if}
          <div class="w-40">
            <Field label="Runs out">
              <Select options={runsOut} bind:value={lasts} />
            </Field>
          </div>
          <Button shape="round" disabled={!capability || (capability === 'work' && !graph) || busy} onclick={add}>Grant</Button>
        </div>
      {/if}
      {#if error}
        <Alert color="danger" size="small">{error}</Alert>
      {/if}
    </div>
  </ModalBody>
</Dialog>
