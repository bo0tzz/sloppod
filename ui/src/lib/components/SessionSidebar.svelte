<script lang="ts">
  import { IconButton } from '@immich/ui';
  import { mdiClose } from '@mdi/js';
  import type { Snippet } from 'svelte';
  import type { WorkDone } from '#lib/api.ts';
  import WorkDoneList from './WorkDoneList.svelte';

  let {
    waiting,
    hasWaiting,
    workDone,
    docked,
    onClose,
  }: {
    /** What waits on the user: questions, permission requests, highlights and open proposals. */
    waiting: Snippet;
    hasWaiting: boolean;
    workDone: WorkDone[];
    /** Beside the chat; otherwise over it, on a narrow screen. */
    docked: boolean;
    onClose: () => void;
  } = $props();
</script>

<aside
  class={[
    'border-light-200 bg-light immich-scrollbar flex w-96 max-w-full shrink-0 flex-col gap-4 overflow-y-auto border-s p-3',
    !docked && 'absolute inset-y-0 end-0 z-10 shadow-xl',
  ]}
  aria-label="Session details"
  data-testid="side-panel"
>
  <div class="flex items-center justify-between">
    <h2 class="text-sm font-semibold">{hasWaiting ? 'Waiting on you' : 'Nothing waiting on you'}</h2>
    <IconButton icon={mdiClose} aria-label="Close the sidebar" title="Close the sidebar" size="small" shape="round" variant="ghost" color="secondary" onclick={onClose} />
  </div>
  {#if hasWaiting}
    <div class="flex flex-col gap-2" data-testid="waiting-items">
      {@render waiting()}
    </div>
  {/if}
  <section class="mt-auto flex flex-col gap-2 border-t pt-3" aria-label="Work done">
    <h2 class="text-sm font-semibold">Work done</h2>
    <WorkDoneList items={workDone} />
  </section>
</aside>
