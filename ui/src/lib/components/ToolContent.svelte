<script lang="ts">
  import { Icon } from '@immich/ui';
  import { mdiFileOutline } from '@mdi/js';
  import type { ToolCallContent, ToolCallLocation } from '#lib/acp/types.ts';
  import { diffTexts } from '#lib/diff.ts';
  import { hasInput } from '#lib/presentation.ts';
  import DiffView from './DiffView.svelte';

  let {
    content,
    locations = [],
    terminals = {},
    rawInput,
    wrap = false,
  }: {
    content: ToolCallContent[];
    locations?: ToolCallLocation[];
    terminals?: Record<string, string>;
    rawInput?: unknown;
    /** Wrap diff lines rather than scroll them sideways. */
    wrap?: boolean;
  } = $props();

  // Adapters fence command output (```console … ```), but send descriptions and short results as plain text.
  function unfence(text: string): string | null {
    const match = /^\s*```[\w-]*\n([\s\S]*?)\n?```\s*$/.exec(text);
    return match ? match[1] : null;
  }

  const input = $derived(rawInput && typeof rawInput === 'object' ? (rawInput as Record<string, unknown>) : null);
  const command = $derived(typeof input?.command === 'string' ? input.command : null);
  const description = $derived(typeof input?.description === 'string' ? input.description : null);
  const showInput = $derived(content.length === 0 && hasInput(rawInput) && command === null);
</script>

<div class="flex flex-col gap-2">
  {#if command !== null}
    {#if description}
      <p class="text-muted text-xs">{description}</p>
    {/if}
    <pre class="tool-output" data-testid="tool-command">$ {command}</pre>
  {/if}
  {#if locations.length > 0}
    <ul class="flex flex-wrap gap-1.5" data-testid="locations">
      {#each locations as location, index (index)}
        <li class="bg-light-100 text-muted inline-flex max-w-full items-center gap-1 rounded-md px-2 py-0.5 font-mono text-xs">
          <Icon icon={mdiFileOutline} class="shrink-0" aria-hidden />
          <span class="truncate" title={location.path}>{location.path}{location.line ? `:${location.line}` : ''}</span>
        </li>
      {/each}
    </ul>
  {/if}

  {#each content as block, index (index)}
    {#if block.type === 'diff'}
      <DiffView diff={diffTexts(block.path, block.oldText, block.newText)} {wrap} />
    {:else if block.type === 'terminal'}
      <pre class="tool-output" data-testid="tool-output">{terminals[block.terminalId] ?? '(no output yet)'}</pre>
    {:else if block.content.type === 'text'}
      {@const output = unfence(block.content.text)}
      {#if output === null}
        <p class="text-sm break-words whitespace-pre-wrap" data-testid="tool-text">{block.content.text}</p>
      {:else}
        <pre class="tool-output" data-testid="tool-output">{output}</pre>
      {/if}
    {:else if block.content.type === 'image'}
      <img src={block.content.uri ?? `data:${block.content.mimeType};base64,${block.content.data}`} alt="" class="max-h-80 rounded-lg" />
    {:else if block.content.type === 'resource_link'}
      <a href={block.content.uri} target="_blank" rel="noopener noreferrer" class="text-primary text-sm underline">{block.content.name}</a>
    {:else if block.content.type === 'resource' && block.content.resource.text !== undefined}
      <pre class="tool-output">{block.content.resource.text}</pre>
    {/if}
  {/each}

  {#if showInput}
    <pre class="tool-output">{JSON.stringify(rawInput, null, 2)}</pre>
  {/if}
</div>

<style>
  .tool-output {
    max-height: 24rem;
    overflow: auto;
    border-radius: 0.5rem;
    padding: 0.5rem 0.75rem;
    font-family: var(--font-mono);
    font-size: 0.75rem;
    line-height: 1.25rem;
    white-space: pre;
    background: var(--immich-ui-light-100);
    border: 1px solid var(--immich-ui-light-200);
  }
</style>
