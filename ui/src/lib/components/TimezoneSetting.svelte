<script lang="ts">
  import { Alert, Field, Heading, LoadingSpinner, Select, Text, toastManager } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api } from '#lib/api.ts';
  import { browserTimezone, timezones } from '#lib/timezone.ts';

  let timezone = $state<string | null>(null);
  let loadError = $state<string | null>(null);

  const options = $derived(timezones(timezone).map((zone) => ({ value: zone, label: zone.replaceAll('_', ' ') })));

  onMount(async () => {
    try {
      timezone = (await api.getSettings()).timezone ?? browserTimezone();
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  });

  async function choose(zone: string) {
    const before = timezone;
    timezone = zone;
    try {
      timezone = (await api.updateSettings({ timezone: zone })).timezone;
      toastManager.success(`Your timezone is ${zone}`);
    } catch (e) {
      timezone = before;
      toastManager.danger({ title: "Couldn't save", description: e instanceof Error ? e.message : String(e) });
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
    <div class="max-w-sm">
      <Field label="Timezone">
        <Select {options} value={timezone} onChange={(value) => void choose(value)} />
      </Field>
    </div>
  {/if}
</section>
