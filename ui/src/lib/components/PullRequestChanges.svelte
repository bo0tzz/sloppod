<script lang="ts">
  import { Checkbox, Heading, Icon, Text } from '@immich/ui';
  import { mdiChevronRight, mdiSourceCommit } from '@mdi/js';
  import { parseUnifiedDiff } from '#lib/diff.ts';
  import DiffView from './DiffView.svelte';

  let {
    diff,
    commits,
    wrap = $bindable(),
  }: { diff: string; commits: { sha: string; message: string }[]; wrap: boolean } = $props();

  const parsed = $derived(parseUnifiedDiff(diff));
  const files = $derived(parsed ?? []);
</script>

<section class="flex flex-col gap-2">
  <Heading tag="h3" size="tiny">Commits ({commits.length})</Heading>
  <ul class="border-light-200 divide-light-200 divide-y rounded-xl border text-sm">
    {#each commits as commit (commit.sha)}
      <li class="flex items-start gap-2 px-3 py-2">
        <Icon icon={mdiSourceCommit} class="text-muted mt-0.5 shrink-0" aria-hidden />
        <span class="min-w-0 grow break-words">{commit.message.split('\n')[0]}</span>
        <code class="text-muted shrink-0 font-mono text-xs">{commit.sha.slice(0, 7)}</code>
      </li>
    {:else}
      <li class="text-muted px-3 py-2">No commits.</li>
    {/each}
  </ul>
</section>

<details class="group/changes flex flex-col gap-2" open data-testid="changes">
  <summary class="flex cursor-pointer list-none items-center gap-1">
    <Icon icon={mdiChevronRight} size="1.1rem" class="text-muted transition-transform group-open/changes:rotate-90" aria-hidden />
    <Heading tag="h3" size="tiny">
      Changes
      {#if parsed}
        <span class="text-muted font-normal">
          ({files.length} {files.length === 1 ? 'file' : 'files'},
          <span class="text-success-600">+{files.reduce((sum, file) => sum + file.additions, 0)}</span>
          <span class="text-danger-600">−{files.reduce((sum, file) => sum + file.deletions, 0)}</span>)
        </span>
      {/if}
    </Heading>
  </summary>
  <div class="mt-2 flex justify-end">
    <label class="text-muted flex shrink-0 items-center gap-2 text-xs">
      <Checkbox bind:checked={wrap} aria-label="Wrap lines" size="tiny" />
      Wrap lines
    </label>
  </div>
  {#if parsed === null}
    <pre class="bg-light-100 mt-2 max-h-[60dvh] overflow-auto rounded-lg p-3 font-mono text-xs">{diff}</pre>
  {:else}
    {#each files as file, index (index)}
      <div class="mt-2"><DiffView diff={file} {wrap} /></div>
    {:else}
      <Text color="muted" size="small">Empty diff.</Text>
    {/each}
  {/if}
</details>
