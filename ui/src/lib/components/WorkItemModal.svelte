<script lang="ts">
  import { Alert, Badge, Button, Field, IconButton, Input, ModalBody, Select, Text, Textarea } from '@immich/ui';
  import { mdiArrowDown, mdiArrowUp, mdiClose, mdiFileTreeOutline } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api, type WorkEvent, type WorkItem, type WorkLinkKind, type WorkState } from '#lib/api.ts';
  import { clockTime } from '#lib/format.ts';
  import { describeEvent, linkHref, linkKinds, linkLabel, siblings, stateColor } from '#lib/work.ts';
  import Dialog from './Dialog.svelte';

  /** An item of a work graph (ADR 30): what it is, what it links to and waits on, and its history, all changeable. */
  let {
    itemId,
    items,
    onChanged,
    onClose,
  }: { itemId: string; items: WorkItem[]; onChanged: () => Promise<void>; onClose: () => void } = $props();

  const item = $derived(items.find((i) => i.id === itemId));
  const titleOf = (id: unknown) => items.find((i) => i.id === id)?.title ?? String(id).slice(0, 8);
  const others = $derived(items.filter((i) => i.id !== itemId));
  const parts = $derived(items.filter((i) => i.parent_id === itemId).sort((a, b) => a.rank - b.rank));

  let history = $state<WorkEvent[]>([]);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let editing = $state(false);
  let title = $state('');
  let description = $state('');
  let blocker = $state<string>();
  let linkKind = $state<WorkLinkKind>('issue');
  let linkTarget = $state('');
  let linkRole = $state<'completes' | 'related'>('completes');

  async function loadHistory() {
    try {
      history = await api.workItemHistory(itemId);
    } catch {
      history = [];
    }
  }
  onMount(loadHistory);

  /** Runs a change, then reloads the graph and the history; a refusal is shown, not thrown. */
  async function act(change: () => Promise<unknown>): Promise<boolean> {
    if (busy) return false;
    busy = true;
    error = null;
    try {
      await change();
      await onChanged();
      await loadHistory();
      return true;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      return false;
    } finally {
      busy = false;
    }
  }

  function startEditing() {
    if (!item) return;
    title = item.title;
    description = item.description;
    editing = true;
  }

  async function save() {
    if (await act(() => api.editWorkItem(itemId, { title, description }))) editing = false;
  }

  const stateOptions = ['open', 'active', 'done', 'dropped'].map((s) => ({ value: s, label: s }));
  const parentOptions = $derived([
    { value: '', label: 'Top level' },
    ...others.map((i) => ({ value: i.id, label: i.title })),
  ]);

  /** Moves it one place among its siblings. */
  function shift(by: -1 | 1) {
    if (!item) return;
    const row = siblings(items, item);
    const at = row.findIndex((i) => i.id === itemId);
    const without = row.filter((i) => i.id !== itemId);
    const index = Math.max(0, Math.min(without.length, at + by));
    const after = index === 0 ? null : without[index - 1].id;
    void act(() => api.moveWorkItem(itemId, { after }));
  }

  async function addLink() {
    if (await act(() => api.addWorkLink(itemId, { kind: linkKind, target: linkTarget.trim(), role: linkRole }))) linkTarget = '';
  }

  const undone = $derived(new Set(history.map((e) => e.undoes).filter(Boolean)));
</script>

<Dialog title={item?.title ?? 'Item'} icon={mdiFileTreeOutline} size="medium" {onClose}>
  <ModalBody>
    {#if item}
      <div class="flex flex-col gap-5 pb-4" data-testid="work-item-detail">
        <section class="flex flex-col gap-2">
          {#if editing}
            <Field label="Title"><Input bind:value={title} /></Field>
            <Field label="Description"><Textarea bind:value={description} /></Field>
            <div class="flex gap-2">
              <Button size="small" shape="round" disabled={busy || !title.trim()} onclick={save}>Save</Button>
              <Button size="small" shape="round" variant="ghost" onclick={() => (editing = false)}>Cancel</Button>
            </div>
          {:else}
            <Text size="small" class="whitespace-pre-wrap">{item.description || 'No description.'}</Text>
            <div>
              <Button size="tiny" shape="round" variant="ghost" onclick={startEditing}>Edit</Button>
            </div>
          {/if}
        </section>

        <section class="flex flex-wrap items-end gap-3">
          <div class="w-36">
            <Field label="State">
              <Select
                options={stateOptions}
                value={item.state}
                onChange={(value) => void act(() => api.setWorkState(itemId, value as WorkState))}
              />
            </Field>
          </div>
          <div class="w-52">
            <Field label="Part of">
              <Select
                options={parentOptions}
                value={item.parent_id ?? ''}
                onChange={(value) => void act(() => api.moveWorkItem(itemId, { parent_id: (value as string) || null }))}
              />
            </Field>
          </div>
          <div class="flex items-center gap-1">
            <IconButton icon={mdiArrowUp} aria-label="Move up" size="small" variant="ghost" shape="round" disabled={busy} onclick={() => shift(-1)} />
            <IconButton icon={mdiArrowDown} aria-label="Move down" size="small" variant="ghost" shape="round" disabled={busy} onclick={() => shift(1)} />
          </div>
        </section>

        <section class="flex flex-col gap-2" data-testid="work-links">
          <Text size="small" class="font-semibold">Links</Text>
          {#each item.links as link (link.id)}
            {@const href = linkHref(link)}
            <div class="flex flex-wrap items-center gap-2 text-sm">
              {#if href}
                <a class="underline" {href} target={href.startsWith('http') ? '_blank' : undefined} rel="noreferrer">{linkLabel(link)}</a>
              {:else}
                <span>{linkLabel(link)}</span>
              {/if}
              <Badge size="tiny" shape="round" color="secondary">{link.role}</Badge>
              <Badge size="tiny" shape="round" color={link.status === 'finished' ? 'success' : 'info'}>{link.status}</Badge>
              <IconButton
                icon={mdiClose}
                aria-label="Remove link {linkLabel(link)}"
                size="tiny"
                variant="ghost"
                shape="round"
                color="danger"
                disabled={busy}
                onclick={() => void act(() => api.removeWorkLink(link.id))}
              />
            </div>
          {:else}
            <Text size="small" color="muted">None yet.</Text>
          {/each}
          <div class="flex flex-wrap items-end gap-2">
            <div class="w-36"><Field label="Link"><Select options={[...linkKinds]} bind:value={linkKind} /></Field></div>
            <div class="w-48"><Field label="Target"><Input bind:value={linkTarget} placeholder="owner/repo#12 or an id" /></Field></div>
            <div class="w-32">
              <Field label="Role">
                <Select options={[{ value: 'completes', label: 'completes' }, { value: 'related', label: 'related' }]} bind:value={linkRole} />
              </Field>
            </div>
            <Button size="small" shape="round" disabled={busy || !linkTarget.trim()} onclick={addLink}>Add link</Button>
          </div>
        </section>

        <section class="flex flex-col gap-2" data-testid="work-blockers">
          <Text size="small" class="font-semibold">Blocked by</Text>
          {#each item.blocked_by as id (id)}
            <div class="flex items-center gap-2 text-sm">
              <span>{titleOf(id)}</span>
              <IconButton
                icon={mdiClose}
                aria-label="Unblock from {titleOf(id)}"
                size="tiny"
                variant="ghost"
                shape="round"
                color="danger"
                disabled={busy}
                onclick={() => void act(() => api.removeWorkBlock(id, itemId))}
              />
            </div>
          {:else}
            <Text size="small" color="muted">Nothing.</Text>
          {/each}
          <div class="flex flex-wrap items-end gap-2">
            <div class="w-56">
              <Field label="Add a blocker">
                <Select options={others.filter((i) => !item.blocked_by.includes(i.id)).map((i) => ({ value: i.id, label: i.title }))} bind:value={blocker} />
              </Field>
            </div>
            <Button
              size="small"
              shape="round"
              disabled={busy || !blocker}
              onclick={() => blocker && void act(() => api.addWorkBlock(blocker!, itemId))}>Block</Button
            >
          </div>
          {#if item.blocks.length > 0}
            <Text size="small" color="muted">Blocks {item.blocks.map(titleOf).join(', ')}.</Text>
          {/if}
        </section>

        {#if parts.length > 0}
          <section class="flex flex-col gap-1">
            <Text size="small" class="font-semibold">Parts</Text>
            {#each parts as part (part.id)}
              <div class="flex items-center gap-2 text-sm">
                <span>{part.title}</span>
                <Badge size="tiny" shape="round" color={stateColor[part.state]}>{part.state}</Badge>
              </div>
            {/each}
          </section>
        {/if}

        {#if error}
          <Alert color="danger" size="small">{error}</Alert>
        {/if}

        <section class="flex flex-col gap-1" data-testid="work-history">
          <Text size="small" class="font-semibold">History</Text>
          {#each [...history].reverse() as event (event.id)}
            <div class="flex flex-wrap items-center gap-x-2 text-sm" data-testid="work-event">
              <span class="text-muted text-xs">{clockTime(event.at)}</span>
              <span>{event.actor}{event.automatic ? ' (by the rules)' : ''}: {describeEvent(event, titleOf)}</span>
              {#if !event.automatic && !undone.has(event.id)}
                <Button size="tiny" shape="round" variant="ghost" disabled={busy} onclick={() => void act(() => api.undoWorkEvent(event.id))}>
                  Undo
                </Button>
              {/if}
            </div>
          {/each}
        </section>
      </div>
    {:else}
      <Text color="muted">This item is gone.</Text>
    {/if}
  </ModalBody>
</Dialog>
