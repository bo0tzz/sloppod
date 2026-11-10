<script lang="ts">
  import { Alert, Button, Field, Heading, LoadingSpinner, Select, Text, toastManager } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api } from '#lib/api.ts';
  import { browserTimezone, timezones } from '#lib/timezone.ts';
  import { userTimezone } from '#lib/user-timezone.svelte.ts';

  /** What the runtime has, and what's chosen but not saved yet: like the other settings, it's saved by its button. */
  let timezone = $state<string | null>(null);
  let chosen = $state<string | null>(null);
  let saving = $state(false);
  let loadError = $state<string | null>(null);

  const options = $derived(timezones(timezone).map((zone) => ({ value: zone, label: zone.replaceAll('_', ' ') })));
  const changed = $derived(chosen !== null && chosen !== timezone);

  onMount(async () => {
    try {
      timezone = chosen = (await api.getSettings()).timezone ?? browserTimezone();
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  });

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (!changed || saving || chosen === null) return;
    saving = true;
    try {
      timezone = (await api.updateSettings({ timezone: chosen })).timezone;
      chosen = timezone;
      userTimezone.value = timezone;
      toastManager.success(`Your timezone is ${timezone}`);
    } catch (e) {
      toastManager.danger({ title: "Couldn't save", description: e instanceof Error ? e.message : String(e) });
    } finally {
      saving = false;
    }
  }
</script>

<section class="flex flex-col gap-3" data-testid="timezone-setting" aria-labelledby="timezone-title">
  <div class="flex flex-col gap-1">
    <Heading tag="h2" size="tiny" id="timezone-title">Timezone</Heading>
    <Text color="muted" size="small">
      Agents are told the time in it, and their pods' clocks use it from the next time an agent starts.
    </Text>
  </div>
  {#if loadError}
    <Alert color="danger" size="small" title="Couldn't load your timezone">{loadError}</Alert>
  {:else if timezone === null}
    <div class="flex py-4"><LoadingSpinner /></div>
  {:else}
    <form class="flex flex-col gap-3" onsubmit={save}>
      <div class="max-w-sm">
        <Field label="Timezone">
          <Select {options} value={chosen ?? timezone} onChange={(value) => (chosen = value)} />
        </Field>
      </div>
      <div>
        <Button type="submit" shape="round" loading={saving} disabled={!changed}>Save</Button>
      </div>
    </form>
  {/if}
</section>
