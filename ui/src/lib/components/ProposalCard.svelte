<script lang="ts">
  import { Badge, Button, Icon } from '@immich/ui';
  import { mdiSourcePull } from '@mdi/js';
  import type { Proposal } from '#lib/api.ts';
  import { proposalStatuses } from '#lib/presentation.ts';

  let { proposal, onOpen }: { proposal: Proposal; onOpen: () => void } = $props();

  const status = $derived(proposalStatuses[proposal.status]);
  const pending = $derived(proposal.status === 'pending');
</script>

<section
  class="flex items-center gap-3 rounded-2xl border p-3 {pending ? 'border-primary/40 bg-primary/5' : 'border-light-200'}"
  data-testid="proposal-card"
  data-status={proposal.status}
>
  <Icon icon={mdiSourcePull} class="text-primary shrink-0" size="1.5em" aria-hidden />
  <div class="flex min-w-0 grow flex-col gap-0.5">
    <div class="flex items-center gap-2">
      <span class="text-muted text-xs">Pull request</span>
      <Badge size="tiny" shape="round" color={status.color}>{status.label}</Badge>
    </div>
    <span class="truncate text-sm font-medium" title={proposal.payload.title}>{proposal.payload.title}</span>
    <span class="text-muted truncate font-mono text-xs">
      {proposal.payload.repo}: {proposal.payload.head} → {proposal.payload.base}
    </span>
  </div>
  <Button size="small" shape="round" variant={pending ? 'filled' : 'outline'} color={pending ? 'primary' : 'secondary'} onclick={onOpen}>
    {pending ? 'Review' : 'View'}
  </Button>
</section>
