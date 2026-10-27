<script lang="ts">
  import { Alert, Button, Checkbox, Field, Heading, LoadingSpinner, Select, Text } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api, type StandingApproval } from '#lib/api.ts';

  const kinds = [
    { value: 'github.pr.create', label: 'Pull requests' },
    { value: 'github.issue.create', label: 'Issues' },
    { value: 'sloppod.repo.grant', label: 'Repository access' },
  ];
  const ANY_REPO = '__any__';
  const durations = [
    { value: '', label: 'Until revoked' },
    { value: String(24 * 3600), label: 'For a day' },
    { value: String(7 * 24 * 3600), label: 'For a week' },
    { value: String(30 * 24 * 3600), label: 'For 30 days' },
  ];

  let approvals = $state<StandingApproval[] | null>(null);
  let repos = $state<string[]>([]);
  let loadError = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  let busy = $state(false);

  let kind = $state('github.pr.create');
  let repo = $state(ANY_REPO);
  let autoMerge = $state(true);
  let duration = $state('');

  const kindLabel = (value: string) => kinds.find((k) => k.value === value)?.label ?? value;
  const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

  async function refresh() {
    try {
      approvals = await api.listStandingApprovals();
      loadError = null;
    } catch (e) {
      loadError = errorText(e);
    }
  }

  onMount(() => {
    void refresh();
    api.listRepos().then(
      (list) => (repos = list),
      () => {},
    );
  });

  async function add(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    actionError = null;
    try {
      await api.createStandingApproval({
        kind,
        target: repo === ANY_REPO ? undefined : repo,
        edits: kind === 'github.pr.create' ? { auto_merge: autoMerge } : {},
        expires_at: duration ? new Date(Date.now() + Number(duration) * 1000).toISOString() : undefined,
      });
      await refresh();
    } catch (e) {
      actionError = errorText(e);
    } finally {
      busy = false;
    }
  }

  async function revoke(id: string) {
    actionError = null;
    try {
      await api.revokeStandingApproval(id);
      await refresh();
    } catch (e) {
      actionError = errorText(e);
    }
  }
</script>

<section class="flex flex-col gap-3" data-testid="standing-approvals" aria-labelledby="standing-approvals-title">
  <div class="flex flex-col gap-1">
    <Heading tag="h2" size="tiny" id="standing-approvals-title">Standing approvals</Heading>
    <Text color="muted" size="small">
      Proposals from your sessions that match one of these are approved as soon as they're made, and carried out like
      any other approval.
    </Text>
  </div>

  {#if loadError}
    <Alert color="danger" size="small" title="Couldn't load your standing approvals">{loadError}</Alert>
  {:else if approvals === null}
    <div class="flex py-4"><LoadingSpinner /></div>
  {:else}
    {#if approvals.length > 0}
      <ul class="border-light-200 divide-light-200 divide-y rounded-xl border text-sm">
        {#each approvals as approval (approval.id)}
          <li class="flex items-center gap-3 px-3 py-2" data-testid="standing-approval">
            <span class="min-w-0 grow">
              <span class="font-medium">{kindLabel(approval.kind)}</span>
              on <span class="font-mono">{approval.target ?? 'any repository'}</span>
              {#if approval.session_id}<span class="text-muted">, one session</span>{/if}
              {#if approval.edits.auto_merge}<span class="text-muted">, merged when checks pass</span>{/if}
              {#if approval.expires_at}
                <span class="text-muted">, until {new Date(approval.expires_at).toLocaleString()}</span>
              {/if}
            </span>
            <Button size="small" shape="round" color="danger" variant="outline" onclick={() => revoke(approval.id)}>
              Revoke
            </Button>
          </li>
        {/each}
      </ul>
    {/if}

    <form class="flex flex-wrap items-end gap-3" onsubmit={add}>
      <div class="min-w-40">
        <Field label="Approve">
          <Select options={kinds} value={kind} onChange={(value) => (kind = value)} />
        </Field>
      </div>
      <div class="min-w-48 grow">
        <Field label="On">
          <Select
            options={[{ value: ANY_REPO, label: 'Any repository' }, ...repos.map((r) => ({ value: r, label: r }))]}
            value={repo}
            onChange={(value) => (repo = value)}
          />
        </Field>
      </div>
      <div class="min-w-36">
        <Field label="Duration">
          <Select options={durations} value={duration} onChange={(value) => (duration = value)} />
        </Field>
      </div>
      {#if kind === 'github.pr.create'}
        <label class="flex items-center gap-2 pb-2 text-sm">
          <Checkbox bind:checked={autoMerge} aria-label="Merge when checks pass" />
          Merge when checks pass
        </label>
      {/if}
      <Button type="submit" shape="round" loading={busy}>Add</Button>
    </form>
    {#if actionError}
      <Alert color="danger" size="small">{actionError}</Alert>
    {/if}
  {/if}
</section>
