<script lang="ts">
  import '../app.css';
  import { afterNavigate } from '$app/navigation';
  import { page } from '$app/state';
  import { AppShell, AppShellSidebar, IconButton, TooltipProvider } from '@immich/ui';
  import { mdiCogOutline, mdiFileTreeOutline, mdiMenuOpen, mdiPlus } from '@mdi/js';
  import { onMount, type Snippet } from 'svelte';
  import { account, sourceLink } from '#lib/account.svelte.ts';
  import AppIcon from '#lib/components/AppIcon.svelte';
  import QuotaMeters from '#lib/components/QuotaMeters.svelte';
  import SessionList from '#lib/components/SessionList.svelte';
  import { sessions } from '#lib/sessions.svelte.ts';
  import { sidebar } from '#lib/sidebar.svelte.ts';
  import { adoptBrowserTimezone } from '#lib/timezone.ts';

  let { children }: { children: Snippet } = $props();

  // The home page is the full session list, so the sidebar would only repeat it.
  const showSidebar = $derived(sidebar.available && page.route.id !== '/');

  afterNavigate(() => {
    if (!sidebar.wide) sidebar.open = false;
  });

  onMount(() => {
    account.load();
    void adoptBrowserTimezone();
    return sessions.subscribe();
  });
</script>

<TooltipProvider>
  <AppShell>
    {#if showSidebar}
      <AppShellSidebar open={sidebar.open} class="md:absolute lg:relative">
        <nav
          class="flex h-full w-64 flex-col"
          aria-label="Sessions"
          aria-hidden={!sidebar.open}
          inert={!sidebar.open}
          data-testid="sidebar"
        >
          <div class="flex h-14 shrink-0 items-center justify-between gap-2 border-b ps-4 pe-2">
            <a href="/" class="flex items-center gap-2 font-semibold">
              <AppIcon class="size-6" />
              sloppod
            </a>
            <div class="flex items-center">
              <IconButton href="/new" icon={mdiPlus} aria-label="New session" variant="ghost" shape="round" size="small" />
              {#if !sidebar.wide}
                <IconButton
                  icon={mdiMenuOpen}
                  aria-label="Hide sessions"
                  variant="ghost"
                  shape="round"
                  size="small"
                  color="secondary"
                  onclick={() => (sidebar.open = false)}
                />
              {/if}
            </div>
          </div>
          <div class="min-h-0 grow overflow-y-auto">
            <SessionList compact sessions={sessions.sorted} activeId={page.params.id} />
          </div>
          <QuotaMeters />
          <div class="border-light-200 flex h-12 shrink-0 items-center gap-2 border-t ps-4 pe-2">
            <div class="text-muted flex min-w-0 grow flex-col text-xs">
              <span class="truncate" data-testid="current-user">
                {account.user ? `Signed in as ${account.user}` : ''}
              </span>
              {#if account.version || account.build}
                <a
                  class="truncate opacity-75 hover:underline"
                  href={sourceLink(account.build)}
                  target="_blank"
                  rel="noreferrer"
                  title={account.build ? `Built from ${account.build}` : undefined}
                  data-testid="runtime-build">sloppod {account.version ?? account.build}</a
                >
              {/if}
            </div>
            <IconButton href="/work" icon={mdiFileTreeOutline} aria-label="Work" variant="ghost" shape="round" size="small" color="secondary" />
            <IconButton href="/settings" icon={mdiCogOutline} aria-label="Settings" variant="ghost" shape="round" size="small" color="secondary" />
          </div>
        </nav>
      </AppShellSidebar>
    {/if}
    {@render children()}
  </AppShell>
</TooltipProvider>
