<script lang="ts">
  import { Alert, Button, Field, Heading, LoadingSpinner, Text, Textarea, toastManager } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api } from '#lib/api.ts';

  let saved = $state<string | null>(null);
  let instructions = $state('');
  let loadError = $state<string | null>(null);
  let saving = $state(false);

  const changed = $derived(saved !== null && instructions.trim() !== saved);

  onMount(async () => {
    try {
      saved = (await api.getSettings()).instructions ?? '';
      instructions = saved;
    } catch (e) {
      loadError = e instanceof Error ? e.message : String(e);
    }
  });

  async function save(event: SubmitEvent) {
    event.preventDefault();
    saving = true;
    try {
      saved = (await api.updateSettings({ instructions: instructions.trim() || null })).instructions ?? '';
      instructions = saved;
      toastManager.success('Saved your instructions');
    } catch (e) {
      toastManager.danger({ title: "Couldn't save", description: e instanceof Error ? e.message : String(e) });
    } finally {
      saving = false;
    }
  }
</script>

<section class="flex flex-col gap-3" data-testid="instructions-setting" aria-labelledby="instructions-title">
  <div class="flex flex-col gap-1">
    <Heading tag="h2" size="tiny" id="instructions-title">Your instructions</Heading>
    <Text color="muted" size="small">
      What all your sessions work by, like a CLAUDE.md of your own: how you want comments, commits and pull requests
      written, say. A running session gets a change the next time its agent starts.
    </Text>
  </div>
  {#if loadError}
    <Alert color="danger" size="small" title="Couldn't load your instructions">{loadError}</Alert>
  {:else if saved === null}
    <div class="flex py-4"><LoadingSpinner /></div>
  {:else}
    <form class="flex flex-col items-end gap-3" onsubmit={save}>
      <div class="w-full">
        <Field label="Instructions">
          <Textarea bind:value={instructions} class="min-h-40 font-mono text-sm" placeholder="Markdown, e.g. - Only comment code for intent a reader couldn't infer." />
        </Field>
      </div>
      <Button type="submit" shape="round" loading={saving} disabled={!changed}>Save</Button>
    </form>
  {/if}
</section>
