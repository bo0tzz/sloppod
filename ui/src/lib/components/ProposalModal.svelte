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
  import { mdiChevronRight, mdiOpenInNew } from '@mdi/js';
  import {
    api,
    type IssueDraft,
    type IssuesPayload,
    type IssueUpdate,
    type IssueUpdatesPayload,
    type Proposal,
    type ProposalDetail,
    type ProposalEdits,
    type PullRequestPayload,
    type RepoGrantPayload,
    type StackPayload,
  } from '#lib/api.ts';
  import { diffStats, diffTexts, parseUnifiedDiff } from '#lib/diff.ts';
  import { readStored, writeStored } from '#lib/storage.ts';
  import { absoluteTime } from '#lib/format.ts';
  import { actingAs, proposalKinds, proposalStatus, proposalSummary } from '#lib/presentation.ts';
  import DiffView from './DiffView.svelte';
  import Markdown from './Markdown.svelte';
  import PullRequestChanges from './PullRequestChanges.svelte';
  import ReviewHistory from './ReviewHistory.svelte';

  let {
    proposal,
    previous,
    onChange,
    onDecided,
    onClose,
  }: {
    proposal: Proposal;
    /** The proposal decided just before this one opened in its place, to confirm what became of it. */
    previous?: Proposal;
    onChange: (proposal: Proposal) => void;
    /** A decision landed and didn't fail: the caller moves on to the next proposal waiting, or closes. */
    onDecided: (proposal: Proposal) => void;
    onClose: () => void;
  } = $props();
  const as = $derived(actingAs(proposal));

  // svelte-ignore state_referenced_locally
  const kind = proposal.kind;

  let detail = $state<ProposalDetail | null>(null);
  let loadError = $state<string | null>(null);
  let actionError = $state<string | null>(null);
  let busy = $state<'approve' | 'reject' | null>(null);

  let editing = $state(false);
  // Off by default would hide most of a docs change, whose paragraphs are single long lines.
  let wrapLines = $state(readStored('sloppod.diff.wrap') !== 'false');
  $effect(() => {
    writeStored('sloppod.diff.wrap', String(wrapLines));
  });
  let title = $state('');
  let body = $state('');
  let rejecting = $state(false);
  let autoMerge = $state(false);
  let reason = $state('');
  let drafts = $state<(IssueDraft & { file: boolean })[]>([]);

  const pr = $derived(detail?.kind === 'github.pr.create' ? (detail.payload as PullRequestPayload) : null);
  const grant = $derived(detail?.kind === 'sloppod.repo.grant' ? (detail.payload as RepoGrantPayload) : null);
  const issues = $derived(detail?.kind === 'github.issue.create' ? (detail.payload as IssuesPayload) : null);
  // Updates to pull requests are shown like updates to issues: the same fields, and a draft can be marked ready.
  const issueUpdates = $derived(
    detail?.kind === 'github.issue.update' || detail?.kind === 'github.pr.update'
      ? (detail.payload as IssueUpdatesPayload)
      : null,
  );
  const closeReasons = { completed: 'completed', not_planned: 'not planned', duplicate: 'a duplicate' };
  const closing = (close: NonNullable<IssueUpdate['close']>) => (close === true ? 'close' : `close as ${closeReasons[close]}`);
  const filing = $derived(drafts.filter((draft) => draft.file).length);
  let updateDrafts = $state<(IssueUpdate & { make: boolean })[]>([]);
  const making = $derived(updateDrafts.filter((draft) => draft.make).length);
  // What the issues an update rewrites say now, to show its change against.
  const currentIssue = (number: number) => detail?.current?.find((issue) => issue.number === number);
  const summary = $derived(proposalSummary(detail ?? proposal));
  const stack = $derived(detail?.kind === 'github.stack.create' ? (detail.payload as StackPayload) : null);
  const pending = $derived(detail?.status === 'pending');

  // The proposal changes while the modal is open: it's decided in another tab, passes review, or a review round
  // re-pins it to a new head. Fetch the detail (with its diff) again then, and put the form back to the new payload.
  const version = $derived(`${proposal.status}\n${JSON.stringify(proposal.payload)}`);
  let loaded = '';
  let loads = 0;
  $effect(() => {
    if (version === loaded) return;
    const payloadChanged = !detail || JSON.stringify(detail.payload) !== JSON.stringify(proposal.payload);
    loaded = version;
    void load(payloadChanged);
  });

  async function load(resetForm: boolean) {
    const load = ++loads;
    try {
      const fetched = await api.getProposal(proposal.id);
      if (load !== loads) return;
      detail = fetched;
      loadError = null;
      if (!resetForm) return;
      editing = false;
      autoMerge =
        (fetched.kind === 'github.pr.create' || fetched.kind === 'github.stack.create') &&
        (fetched.payload as { auto_merge?: boolean }).auto_merge === true;
      resetDrafts();
    } catch (error) {
      if (load === loads) loadError = error instanceof Error ? error.message : String(error);
    }
  }

  const diffCounts = (diff: string) => diffStats(parseUnifiedDiff(diff) ?? []);

  function resetDrafts() {
    if (detail?.kind === 'github.issue.update' || detail?.kind === 'github.pr.update') {
      updateDrafts = (detail.payload as IssueUpdatesPayload).updates.map((update) => ({ ...update, make: true }));
    }
    if (detail?.kind !== 'github.issue.create') return;
    drafts = (detail.payload as IssuesPayload).issues.map((issue) => ({ ...issue, labels: [...issue.labels], file: true }));
  }

  /** Records the decision; the modal only stays on this proposal to show a failure. */
  function applyResult(proposal: Proposal) {
    if (detail) detail = { ...detail, ...proposal };
    onChange(proposal);
    if (proposal.status !== 'failed') onDecided(proposal);
  }

  async function approve() {
    if (!detail) return;
    busy = 'approve';
    actionError = null;
    const edits: ProposalEdits = {};
    if (editing && pr && title !== pr.title) edits.title = title;
    if (editing && pr && body !== pr.body) edits.body = body;
    if (issues) edits.issues = drafts.filter((draft) => draft.file).map(({ file: _file, ...issue }) => issue);
    if (issueUpdates) edits.updates = updateDrafts.filter((draft) => draft.make).map(({ make: _make, ...update }) => update);
    if (pr && autoMerge !== (pr.auto_merge === true)) edits.auto_merge = autoMerge;
    if (stack && autoMerge !== (stack.auto_merge === true)) edits.auto_merge = autoMerge;
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

<Modal title={proposalKinds[kind]?.title ?? `Proposal: ${kind}`} icon={summary.icon} size={pr || stack ? 'giant' : issues ? 'large' : 'medium'} {onClose}>
  <ModalBody>
    <div class="flex flex-col gap-5 pb-4" data-testid="proposal-detail">
      {#if previous}
        {@const previousStatus = proposalStatus(previous)}
        {@const previousSummary = proposalSummary(previous)}
        <div data-testid="proposal-previous">
          <Alert color={previousStatus.color} size="small" title="{previousStatus.label}: {previousSummary.label.toLowerCase()}">
            {previousSummary.title}
            {#if previous.result?.url}
              <a href={previous.result.url} target="_blank" rel="noopener noreferrer" class="inline-flex items-center gap-1 underline">
                #{previous.result.number}
                <Icon icon={mdiOpenInNew} aria-hidden />
              </a>
            {/if}
          </Alert>
        </div>
      {/if}
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
          {#if as}
            <Badge color="warning" shape="round" size="small" data-testid="proposal-acts-as" title="Carried out with {as}'s own GitHub token, in their name">
              Acts as {as}
            </Badge>
          {/if}
          {#if pr}
            <span class="text-muted min-w-0 font-mono text-xs break-all">{pr.repo}: {pr.head} → {pr.base}</span>
          {:else if issues}
            <span class="text-muted min-w-0 font-mono text-xs break-all">{issues.repo}</span>
          {:else if stack}
            <span class="text-muted min-w-0 font-mono text-xs break-all">{stack.repo}: {stack.pull_requests.length} pull requests → {stack.base}</span>
          {/if}
        </div>

        {#if detail.status === 'executed' && pr && detail.result}
          <Alert color="success" size="small" title="Pull request opened">
            <a href={detail.result.url} target="_blank" rel="noopener noreferrer" class="inline-flex items-center gap-1 underline" data-testid="proposal-result">
              #{detail.result.number}
              <Icon icon={mdiOpenInNew} aria-hidden />
            </a>
          </Alert>
        {:else if detail.status === 'executed' && stack && detail.result?.pull_requests}
          <Alert color="success" size="small" title="Pull requests opened">
            <ul class="flex flex-col gap-1">
              {#each detail.result.pull_requests as opened (opened.number)}
                <li>
                  <a href={opened.url} target="_blank" rel="noopener noreferrer" class="inline-flex items-center gap-1 underline" data-testid="proposal-result">
                    #{opened.number}
                    <Icon icon={mdiOpenInNew} aria-hidden />
                  </a>
                </li>
              {/each}
            </ul>
            {#if detail.result.message}<p class="mt-1">{detail.result.message}</p>{/if}
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

        {#if detail.review_session_id}
          <ReviewHistory proposalId={detail.id} reviewSessionId={detail.review_session_id} />
        {/if}

        {#if pr}
          {@render pullRequest(detail, pr)}
        {:else if stack}
          {@render stackedPullRequests(detail, stack)}
        {:else if issues}
          {@render issueList()}
        {:else if issueUpdates}
          <ul class="flex flex-col gap-3" data-testid="issue-updates">
            <!-- By position: one proposal may update the same issue twice. -->
            {#each updateDrafts as update, index (index)}
              {@const current = currentIssue(update.number)}
              <li
                class={['border-light-200 flex flex-col gap-1 rounded-xl border p-3', !update.make && 'opacity-60']}
                data-testid="issue-update"
              >
                <div class="flex items-center justify-between gap-2">
                  <Text fontWeight="semi-bold">
                    #{update.number}
                    {#if update.close}<span class="text-muted font-normal">· {closing(update.close)}</span>{/if}
                    {#if update.reopen}<span class="text-muted font-normal">· reopen</span>{/if}
                    {#if update.ready}<span class="text-muted font-normal">· mark ready for review</span>{/if}
                  </Text>
                  {#if pending}
                    <label class="text-muted flex shrink-0 items-center gap-2 text-xs">
                      <Checkbox bind:checked={update.make} aria-label="Make this update" size="tiny" />
                      Make this update
                    </label>
                  {/if}
                </div>
                {#if update.title && current}
                  <Text size="small" data-testid="issue-update-title">Title: <s class="text-muted">{current.title}</s> → {update.title}</Text>
                {:else if update.title}
                  <Text size="small" data-testid="issue-update-title">New title: {update.title}</Text>
                {/if}
                {#if update.add_labels?.length}<Text size="small">Add labels: {update.add_labels.join(', ')}</Text>{/if}
                {#if update.remove_labels?.length}
                  <Text size="small">Remove labels: {update.remove_labels.join(', ')}</Text>
                {/if}
                {#if editing && update.make}
                  <Field label="Comment">
                    <Textarea bind:value={update.comment} />
                  </Field>
                {:else if update.comment}
                  <div class="bg-light-100 rounded-lg p-2 text-sm"><Markdown text={update.comment} /></div>
                {/if}
                {#if current && update.body !== undefined}
                  {@const bodyDiff = diffTexts(`Body of #${update.number}`, current.body, update.body)}
                  {#if bodyDiff.additions + bodyDiff.deletions === 0}
                    <Text size="small" color="muted">The body stays as it is</Text>
                  {:else}
                    <DiffView diff={bodyDiff} wrap />
                  {/if}
                {/if}
                {#if update.body === ''}
                  <Text size="small">Clears the body</Text>
                {:else if update.body}
                  <details>
                    <summary class="text-muted cursor-pointer text-sm">{current ? 'New body, rendered' : 'New body'}</summary>
                    <div class="bg-light-100 mt-1 rounded-lg p-2 text-sm"><Markdown text={update.body} /></div>
                  </details>
                {/if}
              </li>
            {/each}
          </ul>
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
          Proposed {absoluteTime(detail.created_at)}{#if detail.decided_at}, decided {absoluteTime(detail.decided_at)}{/if}{#if detail.decided_by?.startsWith('standing-approval:')}
            by a standing approval{/if}
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
          {#if pr || stack}
            <label class="text-muted mr-auto flex items-center gap-2 text-sm">
              <Checkbox bind:checked={autoMerge} aria-label="Merge when checks pass" />
              {stack ? 'Merge the stack when checks pass' : 'Merge when checks pass'}
            </label>
          {/if}
          <Button color="danger" variant="outline" shape="round" disabled={busy !== null} onclick={() => (rejecting = true)}>
            Reject…
          </Button>
          {#if editing}
            <Button color="secondary" variant="ghost" shape="round" onclick={discardEdits}>Discard edits</Button>
          {:else if pr || issues || issueUpdates}
            <Button color="secondary" variant="outline" shape="round" onclick={startEditing}>Edit</Button>
          {/if}
          <Button
            color="success"
            shape="round"
            loading={busy === 'approve'}
            disabled={busy === 'reject' || (issues !== null && filing === 0) || (issueUpdates !== null && making === 0)}
            onclick={approve}
          >
            {#if issues}
              File {filing} {filing === 1 ? 'issue' : 'issues'}
            {:else if issueUpdates}
              Make {making} {making === 1 ? 'update' : 'updates'}
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

{#snippet stackedPullRequests(detail: ProposalDetail, stack: StackPayload)}
  <Text color="muted" size="small">
    Each pull request merges into the one before it, the first into <span class="font-mono">{stack.base}</span>. Approving
    opens them all and links them as a stack on GitHub.
  </Text>
  {#each stack.pull_requests as layer, index (layer.head)}
    {@const changes = detail.layers?.[index]}
    {@const counts = changes ? diffCounts(changes.diff) : null}
    <details class="group/layer border-light-200 rounded-2xl border" data-testid="stack-layer">
      <summary class="flex cursor-pointer list-none items-start gap-2 p-3">
        <Icon icon={mdiChevronRight} size="1.25rem" class="text-muted mt-0.5 shrink-0 transition-transform group-open/layer:rotate-90" aria-hidden />
        <div class="flex min-w-0 grow flex-col gap-1">
          <Heading tag="h2" size="tiny">{index + 1}. {layer.title}</Heading>
          <span class="text-muted font-mono text-xs break-all">
            {layer.head} → {layer.base}{#if counts}
              <span class="ms-2 font-sans">
                <span class="text-success-600">+{counts.additions}</span>
                <span class="text-danger-600">−{counts.deletions}</span>
              </span>{/if}
          </span>
        </div>
      </summary>
      <div class="flex flex-col gap-3 px-3 pb-3">
        {#if layer.body.trim()}
          <div class="bg-light-100 rounded-xl p-3 text-sm">
            <Markdown text={layer.body} />
          </div>
        {/if}
        {#if changes}
          <PullRequestChanges diff={changes.diff} commits={changes.commits} bind:wrap={wrapLines} />
        {/if}
      </div>
    </details>
  {/each}
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

  <PullRequestChanges diff={detail.diff ?? ''} commits={detail.commits ?? []} bind:wrap={wrapLines} />
{/snippet}
