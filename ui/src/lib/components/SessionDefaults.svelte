<script lang="ts">
  import { Alert, Button, Field, Heading, LoadingSpinner, Select, Text, toastManager } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api, type ReviewPolicy, type Settings } from '#lib/api.ts';
  import ModelField from './ModelField.svelte';

  let settings = $state<Settings | null>(null);
  let loadError = $state<string | null>(null);
  let saving = $state(false);
  let model = $state('');
  let subSessionModel = $state('');
  let review = $state<ReviewPolicy>('off');

  const changed = $derived(
    model.trim() !== (settings?.model ?? '') ||
      subSessionModel.trim() !== (settings?.sub_session_model ?? '') ||
      review !== (settings?.review ?? 'off'),
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
    saving = true;
    try {
      load(
        await api.updateSettings({
          model: model.trim() || null,
          sub_session_model: subSessionModel.trim() || null,
          review,
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
        <Field label="Review by Codex" description="Another model reviews proposals before you see them; needs a ChatGPT login.">
          <Select options={reviewOptions} value={review} onChange={(value) => (review = value as ReviewPolicy)} />
        </Field>
      </div>
      <Button type="submit" shape="round" loading={saving} disabled={!changed}>Save</Button>
    </form>
  {/if}
</section>
