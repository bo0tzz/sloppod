<script lang="ts">
  import { Alert, Button, Heading, LoadingSpinner, Text } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api, type StandingApproval } from '#lib/api.ts';
  import { sessions } from '#lib/sessions.svelte.ts';

  const kinds = [
    { value: 'github.pr.create', label: 'Pull requests' },
    { value: 'github.issue.create', label: 'Issues' },
    { value: 'sloppod.repo.grant', label: 'Repository access' },
  ];

  let approvals = $state<StandingApproval[] | null>(null);
  let loadError = $state<string | null>(null);
  let actionError = $state<string | null>(null);

  const kindLabel = (value: string) => kinds.find((k) => k.value === value)?.label ?? value;
  const sessionTitle = (id?: string) => sessions.list.find((session) => session.id === id)?.title || 'a session';
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
  });

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
      A session whose pull requests you approve automatically, from its menu, is listed here. Its proposals are
      approved as soon as they're made, or once they pass review if it requires one.
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
              from <a class="underline" href="/sessions/{approval.session_id}">{sessionTitle(approval.session_id)}</a>
              {#if approval.target}on <span class="font-mono">{approval.target}</span>{/if}
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

    {#if approvals.length === 0}
      <Text color="muted" size="small">None. Turn one on from a session's menu.</Text>
    {/if}
    {#if actionError}
      <Alert color="danger" size="small">{actionError}</Alert>
    {/if}
  {/if}
</section>
