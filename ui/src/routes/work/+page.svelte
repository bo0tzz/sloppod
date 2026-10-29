<script lang="ts">
  import { Alert, Button, Field, Input, LoadingSpinner, Text } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api, type WorkGraph } from '#lib/api.ts';
  import ChatHeader from '#lib/components/ChatHeader.svelte';

  let graphs = $state<WorkGraph[] | null>(null);
  let error = $state<string | null>(null);
  let name = $state('');
  let busy = $state(false);

  async function load() {
    try {
      graphs = await api.listWorkGraphs();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }
  onMount(load);

  async function create() {
    busy = true;
    error = null;
    try {
      await api.createWorkGraph(name.trim());
      name = '';
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<svelte:head>
  <title>Work · sloppod</title>
</svelte:head>

<div class="flex h-full min-h-0 flex-col">
  <ChatHeader title="Work" subtitle="Your work graphs" mono={false} />
  <div class="min-h-0 grow overflow-y-auto">
    <div class="mx-auto flex w-full max-w-2xl flex-col gap-6 px-4 py-6">
      {#if graphs === null && !error}
        <div class="flex justify-center py-12"><LoadingSpinner size="large" /></div>
      {:else if graphs}
        <ul class="flex flex-col gap-2" data-testid="work-graphs">
          {#each graphs as graph (graph.id)}
            <li><a class="font-medium hover:underline" href="/work/{graph.id}">{graph.name}</a></li>
          {:else}
            <Text color="muted" size="small">No work graphs yet.</Text>
          {/each}
        </ul>
      {/if}
      <form
        class="flex flex-wrap items-end gap-2"
        onsubmit={(event) => {
          event.preventDefault();
          void create();
        }}
      >
        <div class="w-64"><Field label="New graph"><Input bind:value={name} placeholder="sloppod" /></Field></div>
        <Button type="submit" shape="round" disabled={busy || !name.trim()}>Create</Button>
      </form>
      {#if error}
        <Alert color="danger" size="small">{error}</Alert>
      {/if}
    </div>
  </div>
</div>
