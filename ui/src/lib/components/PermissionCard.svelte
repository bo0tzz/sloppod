<script lang="ts">
  import { Button, Icon, type Color, type Variants } from '@immich/ui';
  import { mdiShieldAlertOutline } from '@mdi/js';
  import type { PermissionOption } from '#lib/acp/types.ts';
  import type { ToolItem } from '#lib/acp/transcript.svelte.ts';
  import { hasInput, toolKindIcons, toolTitle } from '#lib/presentation.ts';
  import type { PendingPermission } from '#lib/session.svelte.ts';
  import ToolContent from './ToolContent.svelte';

  let {
    request,
    tool,
    onAnswer,
  }: { request: PendingPermission; tool?: ToolItem; onAnswer: (optionId: string) => void } = $props();

  // The request may carry only part of the tool call; fill the rest from the transcript.
  const call = $derived(request.toolCall);
  const title = $derived(toolTitle(call.title ?? tool?.title ?? 'Tool call').title);
  const kind = $derived(call.kind ?? tool?.kind ?? 'other');
  const content = $derived(call.content ?? tool?.content ?? []);
  const locations = $derived(call.locations ?? tool?.locations ?? []);
  const rawInput = $derived(call.rawInput ?? tool?.rawInput);

  const optionStyles: Record<PermissionOption['kind'], { color: Color; variant: Variants }> = {
    allow_once: { color: 'primary', variant: 'filled' },
    allow_always: { color: 'primary', variant: 'outline' },
    reject_once: { color: 'danger', variant: 'outline' },
    reject_always: { color: 'danger', variant: 'ghost' },
  };
</script>

<section
  class="border-warning-300 bg-warning-50 dark:bg-warning-100/40 flex flex-col gap-2 rounded-2xl border p-3"
  data-testid="permission-request"
  aria-label="Permission request"
>
  <header class="flex items-center gap-2 text-sm">
    <Icon icon={mdiShieldAlertOutline} class="text-warning-700 shrink-0" size="1.25em" aria-hidden />
    <span class="font-semibold">Permission needed</span>
  </header>
  <div class="flex items-center gap-2 text-sm">
    <Icon icon={toolKindIcons[kind] ?? toolKindIcons.other} class="text-muted shrink-0" aria-hidden />
    <span class="min-w-0 break-words {kind === 'execute' ? 'font-mono text-xs' : ''}">{title}</span>
  </div>
  {#if content.length > 0 || locations.length > 0 || hasInput(rawInput)}
    <div class="max-h-[40dvh] overflow-auto">
      <ToolContent {content} {locations} {rawInput} />
    </div>
  {/if}
  <div class="flex flex-wrap justify-end gap-2 pt-1">
    {#each request.options as option (option.optionId)}
      {@const style = optionStyles[option.kind] ?? { color: 'secondary', variant: 'outline' }}
      <Button
        size="small"
        shape="round"
        color={style.color}
        variant={style.variant}
        onclick={() => onAnswer(option.optionId)}
      >
        {option.name}
      </Button>
    {/each}
  </div>
</section>
