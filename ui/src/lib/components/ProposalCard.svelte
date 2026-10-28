<script lang="ts">
  import { Badge, Button, Icon, LoadingSpinner } from '@immich/ui';
  import { api, type Proposal } from '#lib/api.ts';
  import { actingAs, proposalStatus, proposalSummary } from '#lib/presentation.ts';

  let {
    proposal,
    onOpen,
    onChange,
  }: { proposal: Proposal; onOpen: () => void; onChange: (proposal: Proposal) => void } = $props();

  const status = $derived(proposalStatus(proposal));
  const summary = $derived(proposalSummary(proposal));
  const as = $derived(actingAs(proposal));
  const pending = $derived(proposal.status === 'pending');
  // Everything there is to review fits on the card; only pull requests need the full review.
  const inline = $derived(pending && proposal.kind === 'sloppod.repo.grant');
  const payload = $derived(proposal.payload as { repo?: string; branch?: string; reason?: string });

  let busy = $state<'approve' | 'reject' | null>(null);
  let error = $state<string | null>(null);

  async function decide(verdict: 'approve' | 'reject') {
    busy = verdict;
    error = null;
    try {
      onChange(verdict === 'approve' ? await api.approveProposal(proposal.id) : await api.rejectProposal(proposal.id));
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause);
    } finally {
      busy = null;
    }
  }
</script>

<section
  class="flex flex-col gap-2 rounded-2xl border p-3 {pending ? 'border-primary/40 bg-primary/5' : 'border-light-200'}"
  data-testid="proposal-card"
  data-kind={proposal.kind}
  data-status={proposal.status}
>
  <div class="flex items-center gap-3">
    <Icon icon={summary.icon} class="text-primary shrink-0" size="1.5em" aria-hidden />
    <div class="flex min-w-0 grow flex-col gap-0.5">
      <div class="flex items-center gap-2">
        <span class="text-muted min-w-0 truncate text-xs">{summary.label}</span>
        <!-- A pending card's Review button already says it's waiting on the user. -->
        {#if !pending}
          <Badge size="tiny" shape="round" color={status.color} class="shrink-0 whitespace-nowrap">{status.label}</Badge>
        {/if}
        {#if as}
          <Badge size="tiny" shape="round" color="warning" class="shrink-0 whitespace-nowrap" data-testid="proposal-acts-as">as {as}</Badge>
        {/if}
      </div>
      <span class="truncate text-sm font-medium" title={summary.title}>{summary.title}</span>
      {#if summary.detail && !inline}
        <span class="text-muted truncate text-xs {proposal.kind === 'github.pr.create' ? 'font-mono' : ''}" title={summary.detail}>
          {summary.detail}
        </span>
      {/if}
    </div>
    {#if proposal.status === 'approved'}
      <span class="text-muted flex shrink-0 items-center gap-2 text-xs" data-testid="proposal-working">
        <LoadingSpinner size="small" />
        Carrying it out…
      </span>
    {:else if !inline}
      <Button size="small" shape="round" variant={pending ? 'filled' : 'outline'} color={pending ? 'primary' : 'secondary'} onclick={onOpen}>
        {pending ? 'Review' : 'View'}
      </Button>
    {/if}
  </div>
  {#if proposal.status === 'failed' && proposal.error}
    <p class="text-danger text-xs" data-testid="proposal-error">{proposal.error}</p>
  {/if}
  {#if inline}
    {#if payload.reason}
      <p class="text-sm whitespace-pre-wrap" data-testid="proposal-reason">{payload.reason}</p>
    {/if}
    <p class="text-muted text-xs">
      Approving lets the session push <span class="font-mono">{payload.branch}</span> to {payload.repo} and propose pull
      requests from it. Merging stays with you.
    </p>
    {#if error}
      <p class="text-danger text-xs">{error}</p>
    {/if}
    <div class="flex justify-end gap-2">
      <Button size="small" shape="round" color="danger" variant="outline" loading={busy === 'reject'} disabled={busy !== null} onclick={() => decide('reject')}>
        Reject
      </Button>
      <Button size="small" shape="round" color="success" loading={busy === 'approve'} disabled={busy !== null} onclick={() => decide('approve')}>
        Approve
      </Button>
    </div>
  {/if}
</section>
