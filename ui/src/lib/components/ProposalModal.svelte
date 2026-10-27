<script lang="ts">
  import {
    Alert,
    Badge,
    Button,
    Checkbox,
    Field,
    Heading,
    Icon,
    Input,
    LoadingSpinner,
    Modal,
    ModalBody,
    ModalFooter,
    Text,
    Textarea,
  } from '@immich/ui';
  import { mdiOpenInNew, mdiSourceCommit } from '@mdi/js';
  import { onMount } from 'svelte';
  import {
    api,
    type IssueDraft,
    type IssuesPayload,
    type Proposal,
    type ProposalDetail,
    type ProposalEdits,
    type PullRequestPayload,
    type RepoGrantPayload,
  } from '#lib/api.ts';
  import { parseUnifiedDiff } from '#lib/diff.ts';
  import { absoluteTime } from '#lib/format.ts';
  import { proposalStatus, proposalSummary } from '#lib/presentation.ts';
  import DiffView from './DiffView.svelte';
  import Markdown from './Markdown.svelte';

  let {
    proposal,
    onChange,
    onClose,
  }: { proposal: Proposal; onChange: (proposal: Proposal) => void; onClose: () => void } = $props();

  const kindTitles: Record<string, string> = {
    'github.pr.create': 'Pull request proposal',
    'github.issue.create': 'Issues proposal',
    'sloppod.repo.grant': 'Repository access',
  };
  // svelte-ignore state_referenced_locally
  const kind = proposal.kind;

  let detail = $state<ProposalDetail | null>(null);
  let loadError = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  let busy = $state<'approve' | 'reject' | null>(null);

  let editing = $state(false);
  let title = $state('');
  let body = $state('');
  let rejecting = $state(false);
  let autoMerge = $state(false);
  let reason = $state('');
  let drafts = $state<(IssueDraft & { file: boolean })[]>([]);

  const pr = $derived(detail?.kind === 'github.pr.create' ? (detail.payload as PullRequestPayload) : null);
  const grant = $derived(detail?.kind === 'sloppod.repo.grant' ? (detail.payload as RepoGrantPayload) : null);
  const issues = $derived(detail?.kind === 'github.issue.create' ? (detail.payload as IssuesPayload) : null);
  const filing = $derived(drafts.filter((draft) => draft.file).length);
  const summary = $derived(proposalSummary(detail ?? proposal));
  const parsed = $derived(pr && detail ? parseUnifiedDiff(detail.diff ?? '') : []);
  const files = $derived(parsed ?? []);
  const totalLines = $derived(files.reduce((sum, file) => sum + file.lines.length, 0));
  const pending = $derived(detail?.status === 'pending');

  onMount(async () => {
    try {
      detail = await api.getProposal(proposal.id);
      autoMerge = detail.kind === 'github.pr.create' && (detail.payload as PullRequestPayload).auto_merge === true;
      resetDrafts();
    } catch (error) {
      loadError = error instanceof Error ? error.message : String(error);
    }
  });

  function resetDrafts() {
    if (detail?.kind !== 'github.issue.create') return;
    drafts = (detail.payload as IssuesPayload).issues.map((issue) => ({ ...issue, labels: [...issue.labels], file: true }));
  }

  /** Records the decision; the modal only stays open to show a failure. */
  function applyResult(proposal: Proposal) {
    if (detail) detail = { ...detail, ...proposal };
    onChange(proposal);
    if (proposal.status !== 'failed') onClose();
  }

  async function approve() {
    if (!detail) return;
    busy = 'approve';
    actionError = null;
    const edits: ProposalEdits = {};
    if (editing && pr && title !== pr.title) edits.title = title;
    if (editing && pr && body !== pr.body) edits.body = body;
    if (issues) edits.issues = drafts.filter((draft) => draft.file).map(({ file: _file, ...issue }) => issue);
    if (pr && autoMerge !== (pr.auto_merge === true)) edits.auto_merge = autoMerge;
    try {
      applyResult(await api.approveProposal(detail.id, edits));
      editing = false;
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    } finally {
      busy = null;
    }
  }

  async function reject() {
    if (!detail) return;
    busy = 'reject';
    actionError = null;
    try {
      applyResult(await api.rejectProposal(detail.id, reason.trim() || undefined));
      rejecting = false;
    } catch (error) {
      actionError = error instanceof Error ? error.message : String(error);
    } finally {
      busy = null;
    }
  }

  function startEditing() {
    if (pr) {
      title = pr.title;
      body = pr.body;
    }
    editing = true;
  }

  function discardEdits() {
    resetDrafts();
    editing = false;
  }
</script>

<Modal title={kindTitles[kind] ?? `Proposal: ${kind}`} icon={summary.icon} size={pr ? 'giant' : issues ? 'large' : 'medium'} {onClose}>
  <ModalBody>
    <div class="flex flex-col gap-5 pb-4" data-testid="proposal-detail">
      {#if loadError}
        <Alert color="danger" title="Couldn't load the proposal" size="small">{loadError}</Alert>
      {:else if !detail}
        <div class="flex justify-center py-10"><LoadingSpinner size="large" /></div>
      {:else}
        {@const status = proposalStatus(detail)}
        <div class="flex flex-wrap items-center gap-2 text-sm">
          <Badge color={status.color} shape="round" size="small" data-testid="proposal-status">{status.label}</Badge>
          {#if pr?.draft}
            <Badge color="secondary" shape="round" size="small">Draft</Badge>
          {/if}
          {#if pr?.auto_merge}
            <Badge color="info" shape="round" size="small" data-testid="proposal-auto-merge">Merges when checks pass</Badge>
          {/if}
          {#if pr}
            <span class="text-muted min-w-0 font-mono text-xs break-all">{pr.repo}: {pr.head} → {pr.base}</span>
          {:else if issues}
            <span class="text-muted min-w-0 font-mono text-xs break-all">{issues.repo}</span>
          {/if}
        </div>

        {#if detail.status === 'executed' && pr && detail.result}
          <Alert color="success" size="small" title="Pull request opened">
            <a href={detail.result.url} target="_blank" rel="noopener noreferrer" class="inline-flex items-center gap-1 underline" data-testid="proposal-result">
              #{detail.result.number}
              <Icon icon={mdiOpenInNew} aria-hidden />
            </a>
          </Alert>
        {:else if detail.status === 'executed' && grant}
          <Alert color="success" size="small" title="Access granted">
            The session can now push to <span class="font-mono">{grant.branch}</span> on {grant.repo}.
          </Alert>
        {:else if detail.status === 'executed' && issues && detail.result?.issues}
          <Alert color="success" size="small" title="Issues filed">
            <ul class="flex flex-col gap-1">
              {#each detail.result.issues as filed (filed.number)}
                <li>
                  <a href={filed.url} target="_blank" rel="noopener noreferrer" class="inline-flex items-center gap-1 underline" data-testid="filed-issue">
                    #{filed.number} {filed.title}
                    <Icon icon={mdiOpenInNew} aria-hidden />
                  </a>
                </li>
              {/each}
              {#each detail.result.failed ?? [] as failure, index (index)}
                <li class="text-danger">{failure.title}: {failure.error}</li>
              {/each}
            </ul>
          </Alert>
        {:else if detail.status === 'failed'}
          <Alert color="danger" size="small" title="Execution failed">{detail.error ?? 'Unknown error'}</Alert>
        {:else if detail.status === 'rejected'}
          <Alert color="secondary" size="small" title="Rejected">{detail.reason || 'No reason given.'}</Alert>
        {/if}

        {#if pr}
          {@render pullRequest(detail, pr)}
        {:else if issues}
          {@render issueList()}
        {:else if grant}
          <div class="flex flex-col gap-3">
            <Heading tag="h2" size="tiny" data-testid="proposal-title">{summary.title}</Heading>
            <div class="flex flex-col gap-1">
              <Text size="small" fontWeight="semi-bold">The agent's reason</Text>
              <p class="bg-light-100 rounded-xl p-3 text-sm whitespace-pre-wrap" data-testid="proposal-reason">
                {grant.reason || 'No reason given.'}
              </p>
            </div>
            <Text color="muted" size="small">
              Approving lets the session push to <span class="font-mono">{grant.branch}</span> on {grant.repo} and propose
              pull requests from it. Merging stays with you.
            </Text>
          </div>
        {:else}
          <div class="flex flex-col gap-2">
            <Heading tag="h2" size="tiny" data-testid="proposal-title">{detail.kind}</Heading>
            <Text color="muted" size="small">This UI doesn't know this kind of proposal yet. Its payload:</Text>
            <pre class="bg-light-100 max-h-[50dvh] overflow-auto rounded-lg p-3 font-mono text-xs" data-testid="proposal-payload">{JSON.stringify(detail.payload, null, 2)}</pre>
          </div>
        {/if}

        <Text color="muted" size="tiny">
          Proposed {absoluteTime(detail.created_at)}{#if detail.decided_at}, decided {absoluteTime(detail.decided_at)}{/if}
        </Text>
      {/if}
    </div>
  </ModalBody>

  <!-- The footer stays mounted: @immich/ui's Modal throws if a footer child unmounts while open. -->
  <ModalFooter>
    <div class="flex w-full flex-col gap-3">
      {#if actionError}
        <Alert color="danger" size="small">{actionError}</Alert>
      {/if}
      {#if !pending}
        <div class="flex justify-end">
          <Button color="secondary" shape="round" onclick={onClose}>Done</Button>
        </div>
      {:else if rejecting}
        <Field label="Reason (optional)">
          <Textarea bind:value={reason} rows={2} name="reason" placeholder="Tell the agent why" />
        </Field>
        <div class="flex justify-end gap-2">
          <Button color="secondary" variant="ghost" shape="round" onclick={() => (rejecting = false)}>Cancel</Button>
          <Button color="danger" shape="round" loading={busy === 'reject'} onclick={reject}>Reject</Button>
        </div>
      {:else}
        <div class="flex flex-wrap items-center justify-end gap-2">
          {#if pr}
            <label class="text-muted mr-auto flex items-center gap-2 text-sm">
              <Checkbox bind:checked={autoMerge} aria-label="Merge when checks pass" />
              Merge when checks pass
            </label>
          {/if}
          <Button color="danger" variant="outline" shape="round" disabled={busy !== null} onclick={() => (rejecting = true)}>
            Reject…
          </Button>
          {#if editing}
            <Button color="secondary" variant="ghost" shape="round" onclick={discardEdits}>Discard edits</Button>
          {:else if pr || issues}
            <Button color="secondary" variant="outline" shape="round" onclick={startEditing}>Edit</Button>
          {/if}
          <Button
            color="success"
            shape="round"
            loading={busy === 'approve'}
            disabled={busy === 'reject' || (issues !== null && filing === 0)}
            onclick={approve}
          >
            {#if issues}
              File {filing} {filing === 1 ? 'issue' : 'issues'}
            {:else}
              {editing ? 'Approve with edits' : 'Approve'}
            {/if}
          </Button>
        </div>
      {/if}
    </div>
  </ModalFooter>
</Modal>

{#snippet issueList()}
  <div class="flex flex-col gap-3">
    {#each drafts as draft, index (index)}
      <section
        class="border-light-200 flex flex-col gap-2 rounded-xl border p-3 {draft.file ? '' : 'opacity-50'}"
        data-testid="issue-draft"
      >
        <div class="flex items-start gap-2">
          {#if pending}
            <Checkbox bind:checked={draft.file} aria-label="File this issue" class="mt-0.5" />
          {/if}
          {#if editing && draft.file}
            <Input bind:value={draft.title} aria-label="Issue title" class="grow" />
          {:else}
            <Heading tag="h3" size="tiny" data-testid="issue-title">{draft.title}</Heading>
          {/if}
        </div>
        {#if draft.labels.length > 0}
          <div class="flex flex-wrap gap-1">
            {#each draft.labels as label (label)}
              <Badge size="tiny" shape="round" color="secondary">{label}</Badge>
            {/each}
          </div>
        {/if}
        {#if editing && draft.file}
          <Textarea bind:value={draft.body} rows={6} aria-label="Issue description" class="font-mono" />
        {:else if draft.body.trim()}
          <div class="bg-light-100 rounded-xl p-3 text-sm">
            <Markdown text={draft.body} />
          </div>
        {/if}
      </section>
    {/each}
  </div>
{/snippet}

{#snippet pullRequest(detail: ProposalDetail, pr: PullRequestPayload)}
  {#if editing}
    <div class="flex flex-col gap-3">
      <Field label="Title">
        <Input bind:value={title} name="title" />
      </Field>
      <Field label="Description">
        <Textarea bind:value={body} rows={10} name="body" class="font-mono" />
      </Field>
    </div>
  {:else}
    <div class="flex flex-col gap-2">
      <Heading tag="h2" size="tiny" data-testid="proposal-title">{pr.title}</Heading>
      {#if pr.body.trim()}
        <div class="bg-light-100 rounded-xl p-3 text-sm">
          <Markdown text={pr.body} />
        </div>
      {:else}
        <Text color="muted" size="small">No description.</Text>
      {/if}
    </div>
  {/if}

  <section class="flex flex-col gap-2">
    <Heading tag="h3" size="tiny">Commits ({detail.commits?.length ?? 0})</Heading>
    <ul class="border-light-200 divide-light-200 divide-y rounded-xl border text-sm">
      {#each detail.commits ?? [] as commit (commit.sha)}
        <li class="flex items-start gap-2 px-3 py-2">
          <Icon icon={mdiSourceCommit} class="text-muted mt-0.5 shrink-0" aria-hidden />
          <span class="min-w-0 grow break-words">{commit.message.split('\n')[0]}</span>
          <code class="text-muted shrink-0 font-mono text-xs">{commit.sha.slice(0, 7)}</code>
        </li>
      {:else}
        <li class="text-muted px-3 py-2">No commits.</li>
      {/each}
    </ul>
  </section>

  <section class="flex flex-col gap-2">
    <Heading tag="h3" size="tiny">
      Changes
      {#if parsed}
        <span class="text-muted font-normal">
          ({files.length} {files.length === 1 ? 'file' : 'files'},
          <span class="text-success-600">+{files.reduce((sum, file) => sum + file.additions, 0)}</span>
          <span class="text-danger-600">−{files.reduce((sum, file) => sum + file.deletions, 0)}</span>)
        </span>
      {/if}
    </Heading>
    {#if parsed === null}
      <pre class="bg-light-100 max-h-[60dvh] overflow-auto rounded-lg p-3 font-mono text-xs">{detail.diff}</pre>
    {:else}
      {#each files as file, index (index)}
        <DiffView diff={file} open={totalLines < 1500} />
      {:else}
        <Text color="muted" size="small">Empty diff.</Text>
      {/each}
    {/if}
  </section>
{/snippet}
