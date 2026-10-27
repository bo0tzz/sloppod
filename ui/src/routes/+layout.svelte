<script lang="ts">
  import '../app.css';
  import { page } from '$app/state';
  import { AppShell, AppShellSidebar, IconButton, TooltipProvider } from '@immich/ui';
  import { mdiCogOutline, mdiPlus } from '@mdi/js';
  import { onMount, type Snippet } from 'svelte';
  import { MediaQuery } from 'svelte/reactivity';
  import { account } from '#lib/account.svelte.ts';
  import AppIcon from '#lib/components/AppIcon.svelte';
  import SessionList from '#lib/components/SessionList.svelte';
  import { sessions } from '#lib/sessions.svelte.ts';

  let { children }: { children: Snippet } = $props();

  const desktop = new MediaQuery('(min-width: 768px)');
  // The home page is the full session list, so the sidebar would only repeat it.
  const sidebarOpen = $derived(desktop.current && page.route.id !== '/');

  onMount(() => {
    account.load();
    return sessions.subscribe();
  });
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
        <div class="min-h-0 grow overflow-y-auto">
          <SessionList compact sessions={sessions.sorted} activeId={page.params.id} />
        </div>
        <div class="border-light-200 flex h-12 shrink-0 items-center gap-2 border-t ps-4 pe-2">
          <span class="text-muted min-w-0 grow truncate text-xs" data-testid="current-user">
            {account.user ? `Signed in as ${account.user}` : ''}
          </span>
          <IconButton href="/settings" icon={mdiCogOutline} aria-label="Settings" variant="ghost" shape="round" size="small" color="secondary" />
        </div>
      </AppShellSidebar>
    {/if}
    {@render children()}
  </AppShell>
</TooltipProvider>
