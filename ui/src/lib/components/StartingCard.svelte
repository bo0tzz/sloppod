<script lang="ts">
  import { Checkbox, Field, Input, LoadingSpinner, Select, Text } from '@immich/ui';

  let {
    title,
    repo,
    capabilities,
    offered,
    repos,
    pending,
    source,
    onTitle,
    onRepo,
    onCapability,
  }: {
    title: string;
    repo: string | null;
    /** The capabilities the new session starts with. */
    capabilities: string[];
    /** The ones that can be chosen: what the runtime suggests for an empty message. */
    offered: string[];
    repos: string[];
    pending: boolean;
    source: 'judgement' | 'match' | null;
    onTitle: (title: string) => void;
    onRepo: (repo: string | null) => void;
    onCapability: (capability: string, on: boolean) => void;
  } = $props();

  const NONE = '';
  const repoOptions = $derived([{ value: NONE, label: 'No repository' }, ...repos.map((name) => ({ value: name, label: name }))]);
</script>

<div class="bg-subtle flex w-full flex-col gap-3 rounded-2xl p-4 text-left" data-testid="starting-card">
  <div class="flex items-center justify-between gap-2">
    <Text size="small" class="font-medium">Starting set</Text>
    <Text color="muted" size="tiny" data-testid="starting-source">
      {#if pending}
        <span class="inline-flex items-center gap-1"><LoadingSpinner size="tiny" /> Suggesting…</span>
      {:else if source === 'judgement'}
        Suggested by the model
      {:else if source === 'match'}
        Matched from your message
      {/if}
    </Text>
  </div>
  <Field label="Title">
    <Input value={title} placeholder="Taken from the first message" autocomplete="off" oninput={(event) => onTitle(event.currentTarget.value)} />
  </Field>
  <Field label="Repository">
    <Select options={repoOptions} value={repo ?? NONE} onChange={(value) => onRepo(value === NONE ? null : value)} />
  </Field>
  {#if offered.length > 0}
    <div class="flex flex-col gap-1">
      <Text size="small" class="font-medium">Access</Text>
      <div class="flex flex-wrap gap-x-4 gap-y-1">
        {#each offered as capability (capability)}
          <label class="flex items-center gap-2 text-sm" data-testid="capability-{capability}">
            <Checkbox
              size="tiny"
              checked={capabilities.includes(capability)}
              aria-label={capability}
              onCheckedChange={(on) => onCapability(capability, on)}
            />
            {capability}
          </label>
        {/each}
      </div>
    </div>
  {/if}
</div>
