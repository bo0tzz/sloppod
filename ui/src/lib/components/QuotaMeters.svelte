<script lang="ts">
  import type { Quota, QuotaWindow } from '#lib/api.ts';
  import { absoluteTime, timeAgo } from '#lib/format.ts';
  import { quotas } from '#lib/quotas.svelte.ts';
  import { spending } from '#lib/spend.svelte.ts';
  import { onMount } from 'svelte';

  /** On a page of its own, as a box in the page's column; by default it closes the sidebar. */
  let { framed = false }: { framed?: boolean } = $props();

  onMount(() => {
    const unsubscribe = [quotas.subscribe(), spending.subscribe()];
    return () => unsubscribe.forEach((stop) => stop());
  });
  const dollars = (amount: number) => `$${amount.toFixed(amount < 1 ? 3 : 2)}`;
  // The sessions and shared keys that spent anything, most recently read first; a few, in the sidebar.
  const spent = $derived(spending.spend.keys.filter((key) => key.spend > 0).slice(0, 5));

  const providers: Record<string, string> = { claude: 'Claude', chatgpt: 'ChatGPT' };
  const windows: Record<string, string> = {
    five_hour: '5-hour',
    '1_day': 'Day',
    seven_day: 'Week',
    seven_day_overage_included: 'Week, with overage',
  };
  const unit: Record<string, number> = { minute: 60, hour: 3600, day: 86400 };
  const named: Record<string, number> = { five_hour: 5 * 3600, seven_day: 7 * 86400, seven_day_overage_included: 7 * 86400 + 1 };

  /** A window's length in seconds, from its name (`five_hour`, or `<n>_<unit>` as the runtime names ChatGPT's); unknown last. */
  function duration(window: string): number {
    if (window in named) return named[window];
    const [, n, u] = /^(\d+)_(minute|hour|day)$/.exec(window) ?? [];
    return n ? Number(n) * unit[u] : Infinity;
  }

  const label = (window: string) => windows[window] ?? window.replaceAll('_', ' ');
  const percent = (window: QuotaWindow) => Math.round((window.utilization ?? 0) * 100);

  /** Shortest window first: the one that resets soonest. */
  function sorted(quota: Quota): QuotaWindow[] {
    return [...quota.windows].sort((a, b) => {
      const [x, y] = [duration(a.window), duration(b.window)];
      return x === y ? 0 : x < y ? -1 : 1;
    });
  }

  function level(quota: Quota, window: QuotaWindow): 'danger' | 'warning' | 'normal' {
    if (window.status === 'rejected' || (quota.limited && percent(window) >= 100)) return 'danger';
    if (window.status === 'warning' || (window.utilization ?? 0) >= 0.8) return 'warning';
    return 'normal';
  }

  const bar = { danger: 'bg-danger', warning: 'bg-warning', normal: 'bg-primary' };
  const text = { danger: 'text-danger', warning: 'text-warning-700', normal: 'text-muted' };
</script>

{#if quotas.list.length > 0 || spent.length > 0}
  <ul
    class="border-light-200 flex shrink-0 flex-col gap-3 {framed ? 'rounded-xl border p-3' : 'border-t px-4 py-2'}"
    aria-label="Usage limits and spend"
  >
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
    {#if spent.length > 0}
      <li class="flex flex-col gap-1 text-xs" data-testid="spend">
        <div class="flex items-baseline justify-between gap-2">
          <span class="min-w-0 truncate font-medium">Model endpoints</span>
          <span class="shrink-0" data-testid="spend-total">{dollars(spending.spend.total)} spent</span>
        </div>
        {#each spent as key (key.key_id)}
          {@const over = key.max_budget !== null && key.spend >= key.max_budget}
          <div class="flex items-baseline justify-between gap-2" data-testid="spend-key" title="As of {absoluteTime(key.observed_at)}">
            <span class="text-muted min-w-0 truncate">{key.name}</span>
            <span class={over ? 'text-danger shrink-0' : 'text-muted shrink-0'}>
              {dollars(key.spend)}{key.max_budget !== null ? ` of ${dollars(key.max_budget)}` : ''}
            </span>
          </div>
        {/each}
      </li>
    {/if}
  </ul>
{/if}
