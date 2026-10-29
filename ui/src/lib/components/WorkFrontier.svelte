<script lang="ts">
  import { Heading, Text } from '@immich/ui';
  import { onMount } from 'svelte';
  import { api, type WorkGraph, type WorkItem } from '#lib/api.ts';

  /** What's ready to start in each work graph (ADR 30), first by rank: the home page's start of a dashboard (#137). */
  const SHOWN = 5;
  let graphs = $state<{ graph: WorkGraph; ready: WorkItem[] }[]>([]);

  onMount(async () => {
    try {
      const listed = await api.listWorkGraphs();
      graphs = await Promise.all(listed.map(async (graph) => ({ graph, ready: await api.workFrontier(graph.id).catch(() => []) })));
    } catch {
      graphs = [];
    }
  });
</script>

{#if graphs.length > 0}
  <section class="flex flex-col gap-3" data-testid="work-frontier">
    <Heading tag="h2" size="tiny">Ready to start</Heading>
    {#each graphs as { graph, ready } (graph.id)}
      <div class="flex flex-col gap-1" data-testid="work-frontier-graph" data-graph={graph.name}>
        <a class="text-sm font-semibold hover:underline" href="/work/{graph.id}">{graph.name}</a>
        {#each ready.slice(0, SHOWN) as item (item.id)}
          <a class="truncate text-sm hover:underline" href="/work/{graph.id}">{item.title}</a>
        {:else}
          <Text size="small" color="muted">Nothing is ready.</Text>
        {/each}
        {#if ready.length > SHOWN}
          <a class="text-muted text-xs hover:underline" href="/work/{graph.id}">and {ready.length - SHOWN} more</a>
        {/if}
      </div>
    {/each}
  </section>
{/if}
