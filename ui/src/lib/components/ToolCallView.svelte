<script lang="ts">
  import { Icon } from '@immich/ui';
  import { mdiChevronRight } from '@mdi/js';
  import type { ToolItem } from '#lib/acp/transcript.svelte.ts';
  import { hasInput, toolKindIcons, toolStatuses, toolTitle } from '#lib/presentation.ts';
  import ToolContent from './ToolContent.svelte';

  let { tool }: { tool: ToolItem } = $props();

  let open = $state(false);
  const status = $derived(toolStatuses[tool.status] ?? toolStatuses.pending);
  const display = $derived(toolTitle(tool.title));
  const hasDetails = $derived(tool.content.length > 0 || tool.locations.length > 0 || hasInput(tool.rawInput));
</script>

<details
  bind:open
  class="group border-light-200 bg-light-50 rounded-xl border"
  data-testid="tool-call"
  data-status={tool.status}
  data-kind={tool.kind}
>
  <summary
    class="flex cursor-pointer list-none items-center gap-2 px-3 py-2 text-sm select-none [&::-webkit-details-marker]:hidden"
  >
    <Icon icon={toolKindIcons[tool.kind] ?? toolKindIcons.other} class="text-muted shrink-0" size="1.1em" aria-hidden />
    <span class="min-w-0 grow truncate {tool.kind === 'execute' ? 'font-mono text-xs' : ''}" title={tool.title}>
      {display.title}
    </span>
    <span class="text-muted hidden shrink-0 text-xs sm:inline">{display.server ?? tool.kind.replace('_', ' ')}</span>
    <Icon icon={status.icon} class="shrink-0 {status.class}" size="1.1em" title={status.label} />
    <Icon
      icon={mdiChevronRight}
      class="text-muted shrink-0 transition-transform group-open:rotate-90 {hasDetails ? '' : 'invisible'}"
      aria-hidden
    />
  </summary>
  {#if open && hasDetails}
    <div class="border-light-200 border-t p-3">
      <ToolContent content={tool.content} locations={tool.locations} terminals={tool.terminals} rawInput={tool.rawInput} />
    </div>
  {/if}
</details>
