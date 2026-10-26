<script lang="ts">
  import { Badge } from '@immich/ui';
  import type { Phase } from '#lib/api.ts';
  import { phases } from '#lib/presentation.ts';

  let { phase, size = 'tiny' }: { phase: Phase; size?: 'tiny' | 'small' } = $props();

  const info = $derived(phases[phase] ?? { label: phase, color: 'secondary', description: '' });
  const live = $derived(phase === 'working' || phase === 'starting');
</script>

<Badge color={info.color} {size} shape="round" title={info.description} class="shrink-0" data-testid="phase" data-phase={phase}>
  {#if live}
    <span class="relative me-0.5 flex size-2">
      <span class="absolute inline-flex size-full animate-ping rounded-full bg-current opacity-60"></span>
      <span class="relative inline-flex size-2 rounded-full bg-current"></span>
    </span>
  {/if}
  {info.label}
</Badge>
