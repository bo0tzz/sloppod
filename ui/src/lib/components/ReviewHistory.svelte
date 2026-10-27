<script lang="ts">
  import { Alert, Badge, Icon, LoadingSpinner, Text } from '@immich/ui';
  import { mdiChevronRight, mdiOpenInNew } from '@mdi/js';
  import { api, type Review } from '#lib/api.ts';

  let { proposalId, reviewSessionId }: { proposalId: string; reviewSessionId: string } = $props();

  // Shown on request (ADR 24): the rounds load when the section is first opened.
  let open = $state(false);
  let reviews = $state<Review[] | null>(null);
  let loadError = $state<string | null>(null);

  const verdicts = {
    pass: { label: 'Passed', color: 'success' },
    changes: { label: 'Changes requested', color: 'warning' },
  } as const;
  const severityColors = { p0: 'danger', p1: 'warning', p2: 'secondary' } as const;

  $effect(() => {
    if (!open || reviews || loadError) return;
    api.listReviews(proposalId).then(
      (loaded) => (reviews = loaded),
      (error) => (loadError = error instanceof Error ? error.message : String(error)),
    );
  });
</script>

<details class="group rounded-xl border border-light-200" bind:open data-testid="review-history">
  <summary class="flex cursor-pointer list-none items-center gap-2 px-3 py-2 text-sm font-medium">
    <Icon icon={mdiChevronRight} size="1.1rem" class="transition-transform group-open:rotate-90" aria-hidden />
    Review by Codex
  </summary>
  <div class="flex flex-col gap-4 px-3 pb-3">
    {#if loadError}
      <Alert color="danger" size="small" title="Couldn't load the review">{loadError}</Alert>
    {:else if !reviews}
      <div class="flex py-2"><LoadingSpinner /></div>
    {:else}
      {#each reviews as review (review.round)}
        <section class="flex flex-col gap-2" data-testid="review-round">
          <div class="flex flex-wrap items-center gap-2">
            <Text size="small" fontWeight="semi-bold">Round {review.round}</Text>
            {#if review.verdict}
              <Badge color={verdicts[review.verdict].color} shape="round" size="small">{verdicts[review.verdict].label}</Badge>
            {:else}
              <Badge color="info" shape="round" size="small">In progress</Badge>
            {/if}
            <span class="text-muted font-mono text-xs">{review.head_sha.slice(0, 7)}</span>
          </div>
          {#if review.replies}
            <div class="text-sm">
              <Text size="tiny" color="muted">The agent's replies</Text>
              <p class="whitespace-pre-wrap">{review.replies}</p>
            </div>
          {/if}
          {#if review.summary}
            <p class="text-sm whitespace-pre-wrap">{review.summary}</p>
          {/if}
          {#if review.resolved.length > 0}
            <Text size="tiny" color="muted">Resolved since the last round: {review.resolved.join(', ')}</Text>
          {/if}
          {#if review.findings.length > 0}
            <ul class="flex flex-col gap-2">
              {#each review.findings as finding, index (index)}
                <li class="bg-light-100 flex flex-col gap-1 rounded-lg p-2 text-sm" data-testid="review-finding">
                  <div class="flex flex-wrap items-center gap-2">
                    <Badge color={severityColors[finding.severity]} shape="round" size="tiny">{finding.severity}</Badge>
                    <span class="text-muted text-xs">{finding.category}</span>
                    <span class="font-medium">{finding.title}</span>
                  </div>
                  {#if finding.path}
                    <span class="font-mono text-xs break-all">{finding.path}{finding.line ? `:${finding.line}` : ''}</span>
                  {/if}
                  <p class="whitespace-pre-wrap">{finding.explanation}</p>
                  {#if finding.suggested_fix}
                    <p class="whitespace-pre-wrap"><span class="text-muted">Fix:</span> {finding.suggested_fix}</p>
                  {/if}
                </li>
              {/each}
            </ul>
          {/if}
        </section>
      {:else}
        <Text color="muted" size="small">No rounds yet.</Text>
      {/each}
      <a href="/sessions/{reviewSessionId}" class="text-primary inline-flex items-center gap-1 text-sm underline">
        Open the review session
        <Icon icon={mdiOpenInNew} aria-hidden />
      </a>
    {/if}
  </div>
</details>
