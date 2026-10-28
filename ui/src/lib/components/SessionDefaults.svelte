<script lang="ts">
  import { Alert, Button, Field, Heading, Input, LoadingSpinner, Select, Text, toastManager } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api, type ReviewPolicy, type Settings } from '#lib/api.ts';

  // The select needs a string for "no model set"; null means the agent's default.
  const AGENT_DEFAULT = '__agent_default__';
  // A model id the agent doesn't list, e.g. one the account has but Claude Code doesn't offer yet; typed in.
  const OTHER = '__other__';

  let settings = $state<Settings | null>(null);
  let loadError = $state<string | null>(null);
  let saving = $state(false);
  let model = $state('');
  let review = $state<ReviewPolicy>('off');
  let typing = $state(false);

  const saved = $derived(settings?.model ?? '');
  const changed = $derived(model.trim() !== saved || review !== (settings?.review ?? 'off'));
  const reviewOptions = [
    { value: 'off', label: 'Only when the agent asks' },
    { value: 'required', label: 'Always, for pull requests' },
  ];
  const options = $derived.by(() => {
    if (!settings) return [];
    const models = settings.models.map((entry) => ({ value: entry.value, label: entry.name }));
    return [{ value: AGENT_DEFAULT, label: 'Agent default' }, ...models, { value: OTHER, label: 'Other…' }];
  });

  const offered = (value: string) => settings?.models.some((entry) => entry.value === value) ?? false;
  // A saved model the agent doesn't list shows as typed in, rather than silently as the default.
  const selected = $derived(typing || (model && !offered(model)) ? OTHER : model || AGENT_DEFAULT);

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
              value={selected}
              onChange={(value) => {
                typing = value === OTHER;
                if (value !== OTHER) model = value === AGENT_DEFAULT ? '' : value;
              }}
            />
          </Field>
          {#if selected === OTHER}
            <Field label="Model id" description="Any model id your account has, e.g. one the agent doesn't list yet.">
              <Input bind:value={model} placeholder="claude-…" autocomplete="off" data-testid="custom-model" />
            </Field>
          {/if}
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
