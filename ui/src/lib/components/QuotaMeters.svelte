<script lang="ts">
  import type { Quota, QuotaWindow } from '#lib/api.ts';
  import { timeAgo } from '#lib/format.ts';
  import { quotas } from '#lib/quotas.svelte.ts';
  import { onMount } from 'svelte';

  onMount(() => quotas.subscribe());

  const providers: Record<string, string> = { claude: 'Claude', chatgpt: 'ChatGPT' };
  const windows: Record<string, string> = { five_hour: 'session', seven_day: 'week' };

  /** The provider, and the credential's name when the owner has more than one of its kind with a quota. */
  function title(quota: Quota): string {
    const provider = providers[quota.kind] ?? quota.kind;
    const shared = quotas.list.filter((other) => other.kind === quota.kind).length > 1;
    return shared ? `${provider} · ${quota.name}` : provider;
  }

  const label = (window: string) => windows[window] ?? window.replaceAll('_', ' ');
  const percent = (window: QuotaWindow) => Math.round((window.utilization ?? 0) * 100);

  /** The window closest to its limit, which is the one that stops work first. */
  function fullest(quota: Quota): QuotaWindow | undefined {
    return quota.windows.reduce<QuotaWindow | undefined>(
      (most, window) => ((window.utilization ?? 0) > (most?.utilization ?? -1) ? window : most),
      undefined,
    );
  }

  function level(quota: Quota, window: QuotaWindow | undefined): 'danger' | 'warning' | 'normal' {
    if (quota.limited || window?.status === 'rejected') return 'danger';
    if (window && (window.status === 'warning' || (window.utilization ?? 0) >= 0.8)) return 'warning';
    return 'normal';
  }

  function detail(quota: Quota): string {
    const lines = quota.windows.map((window) => {
      const resets = window.resets_at ? `, resets ${timeAgo(window.resets_at)}` : '';
      return `${label(window.window)}: ${percent(window)}% used${resets}`;
    });
    if (quota.limited) {
      lines.unshift(`Limit reached${quota.resets_at ? `, resets ${timeAgo(quota.resets_at)}` : ''}`);
    }
    lines.push(`As of ${timeAgo(quota.observed_at)}`);
    return `${providers[quota.kind] ?? quota.kind} (${quota.name})\n${lines.join('\n')}`;
  }

  const bar = { danger: 'bg-danger', warning: 'bg-warning', normal: 'bg-primary' };
  const text = { danger: 'text-danger', warning: 'text-warning-700', normal: 'text-muted' };
</script>

{#if quotas.list.length > 0}
  <ul class="border-light-200 flex shrink-0 flex-col gap-2 border-t px-4 py-2" aria-label="Usage limits">
    {#each quotas.list as quota (quota.credential_id)}
      {@const window = fullest(quota)}
      {@const shown = level(quota, window)}
      <li class="flex flex-col gap-1 text-xs" title={detail(quota)} data-testid="quota" data-level={shown}>
        <div class="flex items-baseline justify-between gap-2">
          <span class="truncate">{title(quota)}</span>
          <span class={text[shown]}>
            {#if quota.limited}
              limit reached
            {:else if window}
              {percent(window)}% of {label(window.window)}
            {/if}
          </span>
        </div>
        <div class="bg-light-200 h-1 overflow-hidden rounded-full">
          <div
            class="h-full rounded-full {bar[shown]}"
            style:width="{quota.limited ? 100 : Math.min(100, window ? percent(window) : 0)}%"
          ></div>
        </div>
      </li>
    {/each}
  </ul>
{/if}
