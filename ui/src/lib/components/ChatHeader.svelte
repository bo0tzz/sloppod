<script lang="ts">
  import { IconButton } from '@immich/ui';
  import { mdiArrowLeft } from '@mdi/js';
  import type { Snippet } from 'svelte';

  let {
    title,
    subtitle,
    mono = true,
    actions,
  }: { title: string; subtitle?: string; mono?: boolean; actions?: Snippet } = $props();
</script>

<header class="border-light-200 flex h-14 shrink-0 items-center gap-1 border-b ps-1 pe-2 md:gap-2 md:ps-4">
  <IconButton
    href="/"
    icon={mdiArrowLeft}
    aria-label="All sessions"
    variant="ghost"
    shape="round"
    color="secondary"
    class="md:hidden"
  />
  <div class="flex min-w-0 grow flex-col">
    <h1 class="truncate text-sm leading-tight font-semibold md:text-base" data-testid="session-title">{title}</h1>
    {#if subtitle}
      <span class="text-muted truncate text-xs {mono ? 'font-mono' : ''}" data-testid="session-subtitle">{subtitle}</span>
    {/if}
  </div>
  {@render actions?.()}
</header>
