<script lang="ts">
  import { Modal, ModalBody, Text } from '@immich/ui';
  import { mdiShieldKeyOutline } from '@mdi/js';
  import type { Grant } from '#lib/api.ts';

  let { grants, onClose }: { grants: Grant[]; onClose: () => void } = $props();
</script>

<Modal title="Grants" icon={mdiShieldKeyOutline} size="medium" {onClose}>
  <ModalBody>
    <div class="flex flex-col gap-3 pb-4" data-testid="grants">
      <Text color="muted" size="small">What this session can reach and change. Anything else is refused.</Text>
      <ul class="flex flex-col gap-2">
        {#each grants as grant, index (index)}
          <li class="flex flex-col">
            <span class="font-mono text-sm font-medium">{grant.capability}</span>
            <span class="text-sm">{grant.description ?? ''}</span>
          </li>
        {/each}
      </ul>
    </div>
  </ModalBody>
</Modal>
