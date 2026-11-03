<script lang="ts">
  import { Badge } from '@immich/ui';
  import type { Phase } from '#lib/api.ts';
  import { phaseShown } from '#lib/presentation.ts';

  let {
    phase,
    commands = null,
    commandLines = [],
    size = 'tiny',
  }: { phase: Phase; commands?: number | null; commandLines?: string[]; size?: 'tiny' | 'small' } = $props();

  const info = $derived(phaseShown(phase, commands, commandLines));
  const live = $derived(phase === 'working' || phase === 'starting');
</script>

<Badge
  color={info.color}
  {size}
  shape="round"
  title={info.description}
  class="shrink-0"
  data-testid="phase"
  data-phase={phase}
  data-commands={commands ?? undefined}
>
  {#if live}
    <span class="relative me-0.5 flex size-2">
      <span class="absolute inline-flex size-full animate-ping rounded-full bg-current opacity-60"></span>
      <span class="relative inline-flex size-2 rounded-full bg-current"></span>
    </span>
  {/if}
  {info.label}
</Badge>
