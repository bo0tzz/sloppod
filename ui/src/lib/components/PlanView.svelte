<script lang="ts">
  import { Icon } from '@immich/ui';
  import { mdiFormatListChecks } from '@mdi/js';
  import type { PlanItem } from '#lib/acp/transcript.svelte.ts';
  import { planStatuses } from '#lib/presentation.ts';

  let { plan }: { plan: PlanItem } = $props();

  const done = $derived(plan.entries.filter((entry) => entry.status === 'completed').length);
</script>

<section class="border-light-200 rounded-xl border px-3 py-2.5" data-testid="plan">
  <header class="text-muted mb-1.5 flex items-center gap-2 text-xs font-medium tracking-wide uppercase">
    <Icon icon={mdiFormatListChecks} size="1.2em" aria-hidden />
    Plan
    <span class="ms-auto tabular-nums normal-case">{done}/{plan.entries.length}</span>
  </header>
  <ol class="flex flex-col gap-1 text-sm">
    {#each plan.entries as entry, index (index)}
      {@const status = planStatuses[entry.status] ?? planStatuses.pending}
      <li class="flex items-start gap-2" data-status={entry.status}>
        <Icon icon={status.icon} class="mt-0.5 shrink-0 {status.class}" size="1.1em" title={entry.status.replace('_', ' ')} />
        <span class={entry.status === 'completed' ? 'text-muted line-through' : entry.status === 'in_progress' ? 'font-medium' : ''}>
          {entry.content}
        </span>
        {#if entry.priority === 'high'}
          <span class="text-warning-600 ms-auto shrink-0 text-xs">high</span>
        {/if}
      </li>
    {/each}
  </ol>
</section>
