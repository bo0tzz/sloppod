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

  /**
   * Scrolling the session list with nowhere left to scroll goes on to the chat (#48), as if the wheel were over it. The
   * wheel is passed on as an event too, so the chat takes it as its reader scrolling, not the browser moving it.
   */
  function overscrollToChat(list: HTMLElement) {
    const onWheel = (event: WheelEvent) => {
      const chat = document.querySelector<HTMLElement>('[data-testid="transcript-scroller"]');
      if (!chat || event.deltaY === 0) return;
      const room = event.deltaY > 0 ? list.scrollHeight - list.clientHeight - list.scrollTop : list.scrollTop;
      if (room >= 1) return;
      chat.dispatchEvent(new WheelEvent('wheel', { deltaY: event.deltaY, bubbles: false }));
      chat.scrollBy({ top: event.deltaY });
    };
    list.addEventListener('wheel', onWheel, { passive: true });
    return { destroy: () => list.removeEventListener('wheel', onWheel) };
  }

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
          <div class="min-h-0 grow overflow-y-auto" use:overscrollToChat>
            <SessionList compact sessions={sessions.sorted} activeId={page.params.id} unknown={sessions.lost} />
          </div>
          <QuotaMeters />
          <div class="border-light-200 flex h-12 shrink-0 items-center gap-2 border-t ps-4 pe-2">
            <div class="text-muted flex min-w-0 grow flex-col text-xs">
              <span class="truncate" data-testid="current-user">
                {account.user ? `Signed in as ${account.user}` : ''}
              </span>
              {#if account.version || account.build}
                <div class="flex min-w-0 items-center gap-1">
                  <a
                    class="truncate opacity-75 hover:underline"
                    href={sourceLink(account.build)}
                    target="_blank"
                    rel="noreferrer"
                    title={account.build ? `Built from ${account.build}` : undefined}
                    data-testid="runtime-build">sloppod {account.version ?? account.build}</a
                  >
                  {#if account.outdated}
                    <!-- The runtime is running a newer build than this page's bundle (#448), so a reload gets it. -->
                    <button
                      type="button"
                      class="bg-warning size-1.5 shrink-0 cursor-pointer rounded-full"
                      title="newer version: reload"
                      aria-label="newer version: reload"
                      data-testid="version-stale"
                      onclick={() => location.reload()}
                    ></button>
                  {/if}
                </div>
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
