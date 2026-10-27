<script lang="ts">
  import { Modal, ModalBody, Text } from '@immich/ui';
  import { mdiSourcePull } from '@mdi/js';
  import type { Proposal } from '#lib/api.ts';
  import ProposalCard from './ProposalCard.svelte';

  let {
    proposals,
    onOpen,
    onChange,
    onClose,
  }: {
    proposals: Proposal[];
    onOpen: (id: string) => void;
    onChange: (proposal: Proposal) => void;
    onClose: () => void;
  } = $props();
</script>

<Modal title="Proposals" icon={mdiSourcePull} size="medium" {onClose}>
  <ModalBody>
    <div class="flex flex-col gap-2 pb-4">
      {#each proposals as proposal (proposal.id)}
        <ProposalCard {proposal} onOpen={() => onOpen(proposal.id)} {onChange} />
      {:else}
        <Text color="muted" size="small">The agent hasn't proposed anything in this session.</Text>
      {/each}
    </div>
  </ModalBody>
</Modal>
