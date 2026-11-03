<script lang="ts">
  import { Badge } from '@immich/ui';
  import type { Phase, Waiting } from '#lib/api.ts';
  import { phaseShown } from '#lib/presentation.ts';

  let {
    phase,
    commands = null,
    commandLines = [],
    waiting,
    unknown = false,
    size = 'tiny',
  }: {
    phase: Phase;
    commands?: number | null;
    commandLines?: string[];
    waiting?: Waiting;
    /** The connection is down: the state was last heard as `phase`, and may have changed since. */
    unknown?: boolean;
    size?: 'tiny' | 'small';
  } = $props();

  const info = $derived(
    unknown
      ? { label: 'Unknown', color: 'secondary' as const, description: "The connection is down: what it's doing shows again once it's back" }
      : phaseShown(phase, commands, commandLines, waiting),
  );
  const live = $derived(!unknown && (phase === 'working' || phase === 'starting'));
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
