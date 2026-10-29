<script lang="ts">
  import { Alert, Button, Field, Heading, Input, LoadingSpinner, Select, Text } from '@immich/ui';
  import { page } from '$app/state';
  import { api, type WorkGraph, type WorkItem } from '#lib/api.ts';
  import ChatHeader from '#lib/components/ChatHeader.svelte';
  import WorkItemModal from '#lib/components/WorkItemModal.svelte';
  import WorkItemRow from '#lib/components/WorkItemRow.svelte';
  import { siblings, states, tree } from '#lib/work.ts';

  const graphId = $derived(page.params.id ?? '');
  let graph = $state<(WorkGraph & { items: WorkItem[] }) | null>(null);
  let ready = $state<WorkItem[]>([]);
  let error = $state<string | null>(null);
  let opened = $state<string | null>(null);
  let title = $state('');
  let parent = $state('');
  let busy = $state(false);
  /** The item being dragged to a new rank. */
  let dragging = $state<string | null>(null);

  async function load() {
    try {
      [graph, ready] = await Promise.all([api.getWorkGraph(graphId), api.workFrontier(graphId)]);
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  $effect(() => {
    if (graphId) void load();
  });

  const items = $derived(graph?.items ?? []);
  const titleOf = (id: string) => items.find((i) => i.id === id)?.title ?? id.slice(0, 8);
  const readyIds = $derived(new Set(ready.map((i) => i.id)));
  const open = (item: WorkItem) => (opened = item.id);

  async function act(change: () => Promise<unknown>) {
    busy = true;
    error = null;
    try {
      await change();
      await load();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function add() {
    const asked = title.trim();
    await act(() => api.addWorkItem(graphId, { title: asked, parent_id: parent || null }));
    if (!error) title = '';
  }

  /** Dropping one item on a sibling takes that sibling's place: after it when coming from above, before it otherwise. */
  function drop(target: WorkItem) {
    const moving = items.find((i) => i.id === dragging);
    dragging = null;
    if (!moving || moving.id === target.id || moving.parent_id !== target.parent_id) return;
    const row = siblings(items, moving);
    const from = row.findIndex((i) => i.id === moving.id);
    const to = row.findIndex((i) => i.id === target.id);
    const without = row.filter((i) => i.id !== moving.id);
    const at = without.findIndex((i) => i.id === target.id) + (from < to ? 1 : 0);
    void act(() => api.moveWorkItem(moving.id, { after: at === 0 ? null : without[at - 1].id }));
  }
</script>

<svelte:head>
  <title>{graph?.name ?? 'Work'} · sloppod</title>
</svelte:head>

<div class="flex h-full min-h-0 flex-col">
  <ChatHeader title={graph?.name ?? 'Work graph'} subtitle="Work graph" mono={false} />
  <div class="min-h-0 grow overflow-y-auto">
    <div class="mx-auto flex w-full max-w-3xl flex-col gap-6 px-4 py-6">
      {#if error}
        <Alert color="danger" size="small">{error}</Alert>
      {/if}
      {#if !graph}
        {#if !error}<div class="flex justify-center py-12"><LoadingSpinner size="large" /></div>{/if}
      {:else}
        <form
          class="flex flex-wrap items-end gap-2"
          onsubmit={(event) => {
            event.preventDefault();
            void add();
          }}
        >
          <div class="w-64"><Field label="New item"><Input bind:value={title} placeholder="What to get done" /></Field></div>
          <div class="w-48">
            <Field label="Part of">
              <Select options={[{ value: '', label: 'Top level' }, ...items.map((i) => ({ value: i.id, label: i.title }))]} bind:value={parent} />
            </Field>
          </div>
          <Button type="submit" shape="round" disabled={busy || !title.trim()}>Add</Button>
        </form>

        <section class="flex flex-col gap-1" data-testid="work-ready">
          <Heading tag="h2" size="tiny">Ready</Heading>
          {#each ready as item (item.id)}
            <WorkItemRow {item} {titleOf} onOpen={open} />
          {:else}
            <Text size="small" color="muted">Nothing is ready to start.</Text>
          {/each}
        </section>

        {#each states as { state, label } (state)}
          {@const inState = items.filter((i) => i.state === state && !readyIds.has(i.id))}
          {#if inState.length > 0}
            <section class="flex flex-col gap-1" data-testid="work-{state}">
              <Heading tag="h2" size="tiny">{label}</Heading>
              {#each inState as item (item.id)}
                <WorkItemRow {item} {titleOf} onOpen={open} />
              {/each}
            </section>
          {/if}
        {/each}

        <section class="flex flex-col gap-1" data-testid="work-tree">
          <Heading tag="h2" size="tiny">Structure</Heading>
          <Text size="small" color="muted">Parts under what they're part of, by rank. Drag an item onto a sibling to rerank it.</Text>
          <ol class="flex flex-col">
            {#each tree(items) as { item, depth } (item.id)}
              <li
                draggable="true"
                class="rounded {dragging === item.id ? 'opacity-50' : ''}"
                data-testid="work-tree-item"
                ondragstart={(event) => {
                  dragging = item.id;
                  event.dataTransfer?.setData('text/plain', item.id);
                }}
                ondragend={() => (dragging = null)}
                ondragover={(event) => event.preventDefault()}
                ondrop={(event) => {
                  event.preventDefault();
                  drop(item);
                }}
              >
                <WorkItemRow {item} {titleOf} {depth} onOpen={open} />
              </li>
            {/each}
          </ol>
        </section>
      {/if}
    </div>
  </div>
</div>

{#if opened && graph}
  <WorkItemModal itemId={opened} {items} onChanged={load} onClose={() => (opened = null)} />
{/if}
