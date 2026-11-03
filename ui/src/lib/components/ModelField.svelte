<script lang="ts">
  import { Field, Input, Select } from '@immich/ui';

  // A model id the agent doesn't list, e.g. one the account has but Claude Code doesn't offer yet; typed in.
  const OTHER = '__other__';

  let {
    label,
    description,
    value = $bindable(),
    models,
    unset,
    choices = [],
    testid,
  }: {
    label: string;
    description?: string;
    /** A model id or one of `choices`; empty for `unset`. */
    value: string;
    models: { value: string; name: string }[];
    /** What an empty value means, e.g. "Agent default". */
    unset: string;
    /** Choices offered besides the models, e.g. `parent`. */
    choices?: { value: string; label: string }[];
    testid?: string;
  } = $props();

  // The select needs a string for "nothing set".
  const UNSET = '__unset__';
  let typing = $state(false);

  const options = $derived([
    { value: UNSET, label: unset },
    ...choices,
    ...models.map((entry) => ({ value: entry.value, label: entry.name })),
    { value: OTHER, label: 'Other…' },
  ]);
  const known = (id: string) => choices.some((c) => c.value === id) || models.some((entry) => entry.value === id);
  // A saved model the agent doesn't list shows as typed in, rather than silently as unset.
  const selected = $derived(typing || (value && !known(value)) ? OTHER : value || UNSET);
  const withoutModels = $derived(
    description ? `${description} The list of models appears after your first session.` : 'The list of models appears after your first session.',
  );
</script>

{#if models.length > 0 || choices.length > 0}
  <Field {label} description={models.length > 0 ? description : withoutModels}>
    <Select
      {options}
      value={selected}
      onChange={(choice) => {
        typing = choice === OTHER;
        if (choice !== OTHER) value = choice === UNSET ? '' : choice;
      }}
    />
  </Field>
  {#if selected === OTHER}
    <Field label="Model id" description="Any model id your account has, e.g. one the agent doesn't list yet.">
      <Input bind:value placeholder="claude-…" autocomplete="off" data-testid={testid ? `${testid}-custom` : 'custom-model'} />
    </Field>
  {/if}
{:else}
  <Field {label} description={withoutModels}>
    <Input bind:value placeholder={unset} autocomplete="off" data-testid={testid} />
  </Field>
{/if}
