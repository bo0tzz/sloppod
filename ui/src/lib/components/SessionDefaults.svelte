<script lang="ts">
  import { Alert, Button, Field, Heading, Input, LoadingSpinner, Select, Text, toastManager } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api, type ReviewPolicy, type Settings } from '#lib/api.ts';

  // The select needs a string for "no model set"; null means the agent's default.
  const AGENT_DEFAULT = '__agent_default__';

  let settings = $state<Settings | null>(null);
  let loadError = $state<string | null>(null);
  let saving = $state(false);
  let model = $state('');
  let review = $state<ReviewPolicy>('off');

  const saved = $derived(settings?.model ?? '');
  const changed = $derived(model.trim() !== saved || review !== (settings?.review ?? 'off'));
  const reviewOptions = [
    { value: 'off', label: 'Only when the agent asks' },
    { value: 'required', label: 'Always, for pull requests' },
  ];
  const options = $derived.by(() => {
    if (!settings) return [];
    const models = settings.models.map((entry) => ({ value: entry.value, label: entry.name }));
    // Keep a saved model that the agent no longer offers visible instead of silently showing the default.
    if (settings.model && !models.some((entry) => entry.value === settings!.model)) {
      models.push({ value: settings.model, label: settings.model });
    }
    return [{ value: AGENT_DEFAULT, label: 'Agent default' }, ...models];
  });

  onMount(async () => {
    try {
      settings = await api.getSettings();
      model = settings.model ?? '';
      review = settings.review;
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  });

  async function save(event: SubmitEvent) {
    event.preventDefault();
    saving = true;
    try {
      settings = await api.updateSettings({ model: model.trim() || null, review });
      model = settings.model ?? '';
      review = settings.review;
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
        {#if settings.models.length > 0}
          <Field label="Model">
            <Select
              {options}
              value={model || AGENT_DEFAULT}
              onChange={(value) => (model = value === AGENT_DEFAULT ? '' : value)}
            />
          </Field>
        {:else}
          <Field label="Model" description="The list of models appears after your first session.">
            <Input bind:value={model} placeholder="Agent default" autocomplete="off" />
          </Field>
        {/if}
      </div>
      <div class="min-w-48 grow">
        <Field label="Review by Codex" description="Another model reviews proposals before you see them; needs a ChatGPT login.">
          <Select options={reviewOptions} value={review} onChange={(value) => (review = value as ReviewPolicy)} />
        </Field>
      </div>
      <Button type="submit" shape="round" loading={saving} disabled={!changed}>Save</Button>
    </form>
  {/if}
</section>
