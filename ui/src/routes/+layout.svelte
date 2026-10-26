<script lang="ts">
  import '../app.css';
  import { page } from '$app/state';
  import { AppShell, AppShellSidebar, IconButton, TooltipProvider } from '@immich/ui';
  import { mdiPlus } from '@mdi/js';
  import { onMount, type Snippet } from 'svelte';
  import { MediaQuery } from 'svelte/reactivity';
  import AppIcon from '#lib/components/AppIcon.svelte';
  import SessionList from '#lib/components/SessionList.svelte';
  import { sessions } from '#lib/sessions.svelte.ts';

  let { children }: { children: Snippet } = $props();

  const desktop = new MediaQuery('(min-width: 768px)');
  // The home page is the full session list, so the sidebar would only repeat it.
  const sidebarOpen = $derived(desktop.current && page.route.id !== '/');

  onMount(() => sessions.subscribe());
</script>

<TooltipProvider>
  <AppShell>
    {#if sidebarOpen}
      <AppShellSidebar class="flex flex-col">
        <div class="flex h-14 shrink-0 items-center justify-between gap-2 border-b ps-4 pe-2">
          <a href="/" class="flex items-center gap-2 font-semibold">
            <AppIcon class="size-6" />
            sloppod
          </a>
          <IconButton href="/new" icon={mdiPlus} aria-label="New session" variant="ghost" shape="round" size="small" />
        </div>
        <SessionList compact sessions={sessions.sorted} activeId={page.params.id} />
      </AppShellSidebar>
    {/if}
    {@render children()}
  </AppShell>
</TooltipProvider>
