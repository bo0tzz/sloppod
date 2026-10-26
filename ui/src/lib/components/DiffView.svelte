<script lang="ts">
  import { Button, Icon } from '@immich/ui';
  import { mdiChevronRight } from '@mdi/js';
  import type { FileDiff } from '#lib/diff.ts';

  let { diff, open = true }: { diff: FileDiff; open?: boolean } = $props();

  const PAGE = 400;
  let limit = $state(PAGE);
  const shown = $derived(diff.lines.slice(0, limit));

  const statusLabel = { added: 'new', deleted: 'deleted', renamed: 'renamed', modified: '' } as const;

  const rowClass = {
    add: 'bg-success-500/15',
    del: 'bg-danger-500/15',
    hunk: 'bg-primary/10 text-muted',
    meta: 'text-muted italic',
    ctx: '',
  } as const;

  const marker = { add: '+', del: '-', ctx: ' ', hunk: '', meta: '' } as const;
</script>

<details class="group border-light-200 overflow-hidden rounded-lg border" {open} data-testid="diff">
  <summary
    class="bg-light-100 flex cursor-pointer list-none items-center gap-2 px-3 py-1.5 text-xs select-none [&::-webkit-details-marker]:hidden"
  >
    <Icon icon={mdiChevronRight} class="shrink-0 transition-transform group-open:rotate-90" aria-hidden />
    <span class="min-w-0 grow truncate font-mono" title={diff.path}>
      {#if diff.oldPath}{diff.oldPath} → {/if}{diff.path}
    </span>
    {#if statusLabel[diff.status]}
      <span class="text-muted shrink-0">{statusLabel[diff.status]}</span>
    {/if}
    <span class="text-success-600 shrink-0 font-mono">+{diff.additions}</span>
    <span class="text-danger-600 shrink-0 font-mono">−{diff.deletions}</span>
  </summary>
  <div class="overflow-x-auto">
    <table class="w-full border-collapse font-mono text-xs leading-5">
      <tbody>
        {#each shown as line, index (index)}
          <tr class={rowClass[line.type]}>
            {#if line.type === 'hunk' || line.type === 'meta'}
              <td colspan="2" class="px-3 whitespace-pre">{line.text}</td>
            {:else}
              <td class="text-muted w-px border-e border-light-200 px-2 text-end align-top select-none">
                {line.type === 'del' ? line.oldNo : line.newNo}
              </td>
              <td class="pe-3 whitespace-pre"><span class="inline-block w-4 text-center select-none opacity-60">{marker[line.type]}</span>{line.text}</td>
            {/if}
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
  {#if diff.lines.length > limit}
    <div class="border-light-200 flex justify-center border-t p-1">
      <Button size="tiny" variant="ghost" color="secondary" onclick={() => (limit += PAGE * 5)}>
        Show {Math.min(diff.lines.length - limit, PAGE * 5)} more lines
      </Button>
    </div>
  {/if}
</details>
