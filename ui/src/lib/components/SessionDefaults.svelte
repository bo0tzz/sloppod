<script lang="ts">
  import { Alert, Button, Field, Heading, Input, LoadingSpinner, Select, Text, toastManager } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api, type ReviewPolicy, type Settings } from '#lib/api.ts';
  import ModelField from './ModelField.svelte';

  let settings = $state<Settings | null>(null);
  let loadError = $state<string | null>(null);
  let saving = $state(false);
  let model = $state('');
  let subSessionModel = $state('');
  let review = $state<ReviewPolicy>('off');
  // Dollars, as typed; empty for no limit.
  let budget = $state('');
  // Only a plain number above 0 is a budget; anything else typed is an error, never taken for "no limit".
  const budgetValue = $derived(budget.trim() === '' ? null : /^\d+(\.\d+)?$/.test(budget.trim()) && Number(budget) > 0 ? Number(budget) : NaN);
  const budgetInvalid = $derived(Number.isNaN(budgetValue));

  const changed = $derived(
    model.trim() !== (settings?.model ?? '') ||
      subSessionModel.trim() !== (settings?.sub_session_model ?? '') ||
      review !== (settings?.review ?? 'off') ||
      budgetValue !== (settings?.session_budget ?? null),
  );
  const reviewOptions = [
    { value: 'off', label: 'Only when the agent asks' },
    { value: 'required', label: 'Always, for pull requests' },
  ];

  function load(loaded: Settings) {
    settings = loaded;
    model = loaded.model ?? '';
    subSessionModel = loaded.sub_session_model ?? '';
    review = loaded.review;
    budget = loaded.session_budget === null ? '' : String(loaded.session_budget);
  }

  onMount(async () => {
    try {
      load(await api.getSettings());
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  });

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (budgetInvalid) return;
    saving = true;
    try {
      load(
        await api.updateSettings({
          model: model.trim() || null,
          sub_session_model: subSessionModel.trim() || null,
          review,
          session_budget: budgetInvalid ? undefined : budgetValue,
        }),
      );
      toastManager.success('Saved your defaults');
    } catch (e) {
      toastManager.danger({ title: "Couldn't save", description: e instanceof Error ? e.message : String(e) });
    } finally {
      saving = false;
    }
  }
</script>

<section class="flex flex-col gap-3" data-testid="session-defaults" aria-labelledby="session-defaults-title">
  <div class="flex flex-col gap-1">
    <Heading tag="h2" size="tiny" id="session-defaults-title">Defaults for new sessions</Heading>
    <Text color="muted" size="small">Sessions run in auto mode; their sandbox and grants keep that safe.</Text>
  </div>

  {#if loadError}
    <Alert color="danger" size="small" title="Couldn't load your defaults">{loadError}</Alert>
  {:else if !settings}
    <div class="flex py-4"><LoadingSpinner /></div>
  {:else}
    <form class="flex flex-wrap items-end gap-3" onsubmit={save}>
      <div class="min-w-48 grow">
        <ModelField label="Model" bind:value={model} models={settings.models} unset="Agent default" />
      </div>
      <div class="min-w-48 grow">
        <ModelField
          label="Sub-sessions"
          description="What sessions an agent starts run on, unless it names a model."
          bind:value={subSessionModel}
          models={settings.models}
          unset="A cheaper model (Sonnet)"
          choices={[{ value: 'parent', label: 'The same as their parent' }]}
          testid="sub-session-model"
        />
      </div>
      <div class="min-w-48 grow">
        <Field
          label="Budget per session"
          description="The most a session on a LiteLLM operator model may spend, in dollars; LiteLLM refuses it after that. Empty for no limit."
          invalid={budgetInvalid}
        >
          <Input bind:value={budget} inputmode="decimal" placeholder="No limit" autocomplete="off" data-testid="session-budget" />
          {#if budgetInvalid}
            <Text color="danger" size="small" data-testid="session-budget-error">A number of dollars, like 5 or 2.50, or empty for no limit.</Text>
          {/if}
        </Field>
      </div>
      <div class="min-w-48 grow">
        <Field label="Review by Codex" description="Another model reviews proposals before you see them; needs a ChatGPT login.">
          <Select options={reviewOptions} value={review} onChange={(value) => (review = value as ReviewPolicy)} />
        </Field>
      </div>
      <Button type="submit" shape="round" loading={saving} disabled={!changed || budgetInvalid}>Save</Button>
    </form>
  {/if}
</section>
