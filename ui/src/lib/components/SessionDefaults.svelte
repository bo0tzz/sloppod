<script lang="ts">
  import { Alert, Button, Field, Heading, Input, LoadingSpinner, Select, Text, toastManager } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api, type Settings } from '#lib/api.ts';

  // The select needs a string for "no model set"; null means the agent's default.
  const AGENT_DEFAULT = '__agent_default__';

  let settings = $state<Settings | null>(null);
  let loadError = $state<string | null>(null);
  let saving = $state(false);
  let model = $state('');

  const saved = $derived(settings?.model ?? '');
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
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  });

  async function save(event: SubmitEvent) {
    event.preventDefault();
    saving = true;
    try {
      settings = await api.updateSettings({ model: model.trim() || null });
      model = settings.model ?? '';
      toastManager.success('Saved the default model');
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
      <Button type="submit" shape="round" loading={saving} disabled={model.trim() === saved}>Save</Button>
    </form>
  {/if}
</section>
