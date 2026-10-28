<script lang="ts">
  import type { Quota, QuotaWindow } from '#lib/api.ts';
  import { absoluteTime, timeAgo } from '#lib/format.ts';
  import { quotas } from '#lib/quotas.svelte.ts';
  import { onMount } from 'svelte';

  onMount(() => quotas.subscribe());

  const providers: Record<string, string> = { claude: 'Claude', chatgpt: 'ChatGPT' };
  const windows: Record<string, string> = {
    five_hour: '5-hour',
    seven_day: 'Week',
    seven_day_overage_included: 'Week, with overage',
  };
  /** Shortest window first: the one that resets soonest. */
  const order = ['five_hour', 'seven_day', 'seven_day_overage_included'];

  const label = (window: string) => windows[window] ?? window.replaceAll('_', ' ');
  const percent = (window: QuotaWindow) => Math.round((window.utilization ?? 0) * 100);

  function sorted(quota: Quota): QuotaWindow[] {
    const rank = (window: QuotaWindow) => {
      const known = order.indexOf(window.window);
      return known === -1 ? order.length : known;
    };
    return [...quota.windows].sort((a, b) => rank(a) - rank(b));
  }

  function level(quota: Quota, window: QuotaWindow): 'danger' | 'warning' | 'normal' {
    if (window.status === 'rejected' || (quota.limited && percent(window) >= 100)) return 'danger';
    if (window.status === 'warning' || (window.utilization ?? 0) >= 0.8) return 'warning';
    return 'normal';
  }

  const bar = { danger: 'bg-danger', warning: 'bg-warning', normal: 'bg-primary' };
  const text = { danger: 'text-danger', warning: 'text-warning-700', normal: 'text-muted' };
</script>

{#if quotas.list.length > 0}
  <ul class="border-light-200 flex shrink-0 flex-col gap-3 border-t px-4 py-2" aria-label="Usage limits">
    {#each quotas.list as quota (quota.credential_id)}
      <li class="flex flex-col gap-1 text-xs" data-testid="quota" data-limited={quota.limited}>
        <div class="flex items-baseline justify-between gap-2" title="As of {absoluteTime(quota.observed_at)}">
          <span class="min-w-0 truncate font-medium">{quota.name}</span>
          <span class="text-muted shrink-0">{providers[quota.kind] ?? quota.kind}</span>
        </div>
        {#if quota.limited}
          <div class="text-danger" data-testid="quota-limited">
            Limit reached{#if quota.resets_at}, resets {timeAgo(quota.resets_at)}{/if}
          </div>
        {/if}
        {#each sorted(quota) as window (window.window)}
          {@const shown = level(quota, window)}
          <div
            class="flex flex-col gap-0.5"
            data-testid="quota-window"
            data-window={window.window}
            data-level={shown}
            title={window.resets_at ? `Resets ${absoluteTime(window.resets_at)}` : undefined}
          >
            <div class="flex items-baseline justify-between gap-2">
              <span class="text-muted">{label(window.window)}</span>
              <span class={text[shown]}>
                {percent(window)}%{#if window.resets_at}<span class="text-muted">, resets {timeAgo(window.resets_at)}</span>{/if}
              </span>
            </div>
            <div class="bg-light-200 h-1 overflow-hidden rounded-full">
              <div class="h-full rounded-full {bar[shown]}" style:width="{Math.min(100, percent(window))}%"></div>
            </div>
          </div>
        {/each}
      </li>
    {/each}
  </ul>
{/if}
