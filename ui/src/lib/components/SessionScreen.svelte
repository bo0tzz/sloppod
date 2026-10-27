<script lang="ts">
  import { goto } from '$app/navigation';
  import {
    Alert,
    Button,
    ContextMenuButton,
    IconButton,
    LoadingSpinner,
    Text,
    modalManager,
    toastManager,
  } from '@immich/ui';
  import {
    mdiArrowDown,
    mdiCheckDecagramOutline,
    mdiCogOutline,
    mdiPauseCircleOutline,
    mdiPlayCircleOutline,
    mdiMagnifyScan,
    mdiShieldKeyOutline,
    mdiSourcePull,
    mdiSourceRepository,
    mdiTrashCanOutline,
  } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api } from '#lib/api.ts';
  import { SessionView } from '#lib/session.svelte.ts';
  import { sessions } from '#lib/sessions.svelte.ts';
  import AddRepoModal from './AddRepoModal.svelte';
  import ChatHeader from './ChatHeader.svelte';
  import GrantsModal from './GrantsModal.svelte';
  import PermissionCard from './PermissionCard.svelte';
  import QuestionCard from './QuestionCard.svelte';
  import PhaseBadge from './PhaseBadge.svelte';
  import ProposalCard from './ProposalCard.svelte';
  import ProposalListModal from './ProposalListModal.svelte';
  import ProposalModal from './ProposalModal.svelte';
  import SessionComposer from './SessionComposer.svelte';
  import TranscriptView from './TranscriptView.svelte';

  let { id }: { id: string } = $props();

  // The route wraps this component in {#key id}, so capturing the first id is intended.
  // svelte-ignore state_referenced_locally
  const view = new SessionView(id);

  const session = $derived(view.session);
  const title = $derived(session?.title || 'New session');
  const pendingProposals = $derived(view.proposals.filter((proposal) => proposal.status === 'pending'));
  // Approved proposals are carried out in the background; their cards stay until that's done. Failures go to the
  // agent, not here.
  const openProposals = $derived(
    view.proposals.filter((proposal) => proposal.status === 'pending' || proposal.status === 'approved'),
  );
  const showReconnecting = $derived(view.loaded && view.connection !== 'open');
  const activity = $derived.by(() => {
    switch (session?.phase) {
      case 'starting':
        return 'Starting the agent…';
      case 'disconnected':
        return 'Bringing the agent back…';
      case 'waiting':
        return view.questions.length > 0 && view.permissions.length === 0 ? 'Waiting for your answer' : 'Waiting for your decision';
      default:
        return view.turnRunning ? 'Working…' : null;
    }
  });

  let openProposalId = $state<string | null>(null);
  const openProposal = $derived(view.proposals.find((proposal) => proposal.id === openProposalId));
  /** The proposals list: everything (from the menu), or just the open ones (from the bar above the composer). */
  let proposalList = $state<'all' | 'open' | null>(null);
  let showAddRepo = $state(false);
  let showGrants = $state(false);
  /** This session's standing approvals for pull requests; any one of them approves its pull requests. */
  let autoApprovals = $state<string[]>([]);
  let togglingApproval = $state(false);

  async function loadAutoApprovals() {
    const approvals = await api.listStandingApprovals();
    autoApprovals = approvals.filter((a) => a.session_id === id && a.kind === 'github.pr.create').map((a) => a.id);
  }

  const waiting = $derived(view.permissions.length > 0 || view.questions.length > 0 || openProposals.length > 0);
  let width = $state(0);
  // Room for the chat at its full width (max-w-3xl and its padding) with the panel beside it.
  const sidePanel = $derived(width >= 1184);

  let scroller = $state<HTMLElement | null>(null);
  let content = $state<HTMLElement | null>(null);
  let atBottom = $state(true);

  onMount(() => {
    loadAutoApprovals().catch(() => {});
    view.start();
    return () => view.stop();
  });

  function onScroll() {
    if (!scroller) return;
    atBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
  }

  function scrollToBottom(behavior: ScrollBehavior = 'instant') {
    scroller?.scrollTo({ top: scroller.scrollHeight, behavior });
  }

  // Follow new output while the reader is at the bottom, but leave them alone when they scrolled up.
  $effect(() => {
    if (!content || !scroller) return;
    const observer = new ResizeObserver(() => {
      if (atBottom) scrollToBottom();
    });
    observer.observe(content);
    observer.observe(scroller);
    return () => observer.disconnect();
  });

  async function toggleReview() {
    if (!session) return;
    try {
      view.setSession(await api.setSessionReview(id, session.review === 'required' ? 'off' : 'required'));
    } catch (error) {
      toastManager.danger({
        title: "Couldn't change the review setting",
        description: error instanceof Error ? error.message : String(error),
      });
    }
  }

  /** Turns automatic approval on or off as chosen, whatever another tab did since the menu was drawn. */
  async function setAutoApproval(on: boolean) {
    if (togglingApproval) return;
    togglingApproval = true;
    try {
      // Fresh, so another tab's changes count: any live approval keeps approving until all are revoked.
      await loadAutoApprovals();
      if (!on) {
        await Promise.all(autoApprovals.map((approval) => api.revokeStandingApproval(approval)));
      } else if (autoApprovals.length === 0) {
        await api.createStandingApproval({ kind: 'github.pr.create', session_id: id, edits: { auto_merge: true } });
      }
      await loadAutoApprovals();
    } catch (error) {
      toastManager.danger({
        title: "Couldn't change automatic approval",
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      togglingApproval = false;
    }
  }

  async function setParked(parked: boolean) {
    try {
      view.setSession(await (parked ? api.parkSession(id) : api.wakeSession(id)));
    } catch (error) {
      toastManager.danger({
        title: parked ? "Couldn't park the session" : "Couldn't wake the session",
        description: error instanceof Error ? error.message : String(error),
      });
    }
  }

  async function deleteSession() {
    const confirmed = await modalManager.showDialog({
      title: 'Delete session?',
      prompt:
        'It stops and leaves your sessions. For a week you can restore it from Settings, with its files, branch and ' +
        'memory; after that it is gone for good. To just stop it for now, park it instead.',
      confirmText: 'Delete',
      icon: mdiTrashCanOutline,
    });
    if (!confirmed) return;
    try {
      await api.deleteSession(id);
      sessions.remove(id);
      await goto('/');
    } catch (error) {
      toastManager.danger({
        title: "Couldn't delete the session",
        description: error instanceof Error ? error.message : String(error),
      });
    }
  }
</script>

<svelte:head>
  <title>{title} · sloppod</title>
</svelte:head>

{#snippet waitingItems()}
      {#each view.questions as question (question.rpcId)}
        <QuestionCard request={question} onRespond={(response) => view.answerQuestion(question, response)} />
      {/each}
      {#each view.permissions as request (request.rpcId)}
        <PermissionCard
          {request}
          grants={session?.grants ?? []}
          tool={view.transcript.tool(request.toolCall.toolCallId)}
          onAnswer={(optionId) => view.answerPermission(request, optionId)}
        />
      {/each}
      {#if sidePanel}
        {#each openProposals as proposal (proposal.id)}
          <ProposalCard
            {proposal}
            onOpen={() => (openProposalId = proposal.id)}
            onChange={(updated) => view.upsertProposal(updated)}
          />
        {/each}
      {:else if openProposals.length === 1}
        <ProposalCard
          proposal={openProposals[0]}
          onOpen={() => (openProposalId = openProposals[0].id)}
          onChange={(proposal) => view.upsertProposal(proposal)}
        />
      {:else if openProposals.length > 1}
        <!-- Several at once would crowd out the chat, especially on a phone. -->
        <div class="border-primary/40 bg-primary/5 flex items-center gap-3 rounded-2xl border px-3 py-2" data-testid="proposals-bar">
          <span class="grow text-sm">
            {#if pendingProposals.length > 0}
              <span class="font-medium">{pendingProposals.length} {pendingProposals.length === 1 ? 'proposal' : 'proposals'} to review</span>
            {/if}
            {#if openProposals.length > pendingProposals.length}
              <span class="text-muted">
                {pendingProposals.length > 0 ? ', ' : ''}{openProposals.length - pendingProposals.length} being carried out
              </span>
            {/if}
          </span>
          <Button size="small" shape="round" onclick={() => (proposalList = 'open')}>
            {pendingProposals.length > 0 ? 'Review' : 'View'}
          </Button>
        </div>
      {/if}
{/snippet}

{#if view.notFound}
  <div class="mx-auto flex max-w-md flex-col items-center gap-4 px-4 py-16 text-center">
    <Text size="large" fontWeight="semi-bold">Session not found</Text>
    <Text color="muted">It may have been deleted.</Text>
    <Button href="/" shape="round">All sessions</Button>
  </div>
{:else}
  <div class="flex h-full min-h-0 flex-col" data-testid="session-screen">
    <ChatHeader {title} subtitle={session ? (session.repo ?? 'No repository') : undefined} mono={Boolean(session?.repo)}>
      {#snippet actions()}
        {#if session}
          <PhaseBadge phase={session.phase} />
        {/if}
        <IconButton
          icon={mdiSourcePull}
          aria-label="Proposals"
          variant="ghost"
          shape="round"
          color="secondary"
          indicator={pendingProposals.length > 0 ? 'warning' : undefined}
          onclick={() => (proposalList = 'all')}
        />
        <ContextMenuButton
          aria-label="Session actions"
          items={[
            {
              title: 'Grants',
              icon: mdiShieldKeyOutline,
              $if: () => Boolean(session),
              onAction: () => (showGrants = true),
            },
            {
              title: 'Add repository…',
              icon: mdiSourceRepository,
              $if: () => Boolean(session) && session?.phase !== 'stopped',
              onAction: () => (showAddRepo = true),
            },
            {
              title: session?.review === 'required' ? 'Stop requiring review' : 'Require review by Codex',
              icon: mdiMagnifyScan,
              $if: () => session?.kind === 'work',
              onAction: toggleReview,
            },
            {
              title: autoApprovals.length > 0 ? 'Stop approving its pull requests' : 'Approve its pull requests automatically',
              icon: mdiCheckDecagramOutline,
              $if: () => session?.kind === 'work',
              onAction: () => setAutoApproval(autoApprovals.length === 0),
            },
            {
              title: 'Park',
              icon: mdiPauseCircleOutline,
              $if: () => session?.phase === 'idle' || session?.phase === 'disconnected',
              onAction: () => setParked(true),
            },
            {
              title: 'Wake',
              icon: mdiPlayCircleOutline,
              $if: () => session?.phase === 'parked',
              onAction: () => setParked(false),
            },
            { title: 'Settings', icon: mdiCogOutline, href: '/settings' },
            { title: 'Delete session', icon: mdiTrashCanOutline, color: 'danger', onAction: deleteSession },
          ]}
        />
      {/snippet}
    </ChatHeader>

    {#if showReconnecting}
      <div
        class="bg-warning-100 text-warning-800 flex shrink-0 items-center justify-center gap-2 px-3 py-1 text-xs"
        role="status"
        data-testid="reconnecting"
      >
        <LoadingSpinner size="tiny" />
        Connection lost, reconnecting…
      </div>
    {/if}

    <div class="flex min-h-0 grow" bind:clientWidth={width}>
      <div class="flex min-w-0 grow flex-col">
        <div class="relative min-h-0 grow">
          <div bind:this={scroller} onscroll={onScroll} class="immich-scrollbar h-full overflow-y-auto">
            <div bind:this={content} class="mx-auto flex w-full max-w-3xl flex-col gap-3 px-3 py-4 md:px-4">
              {#if !view.loaded}
                <div class="flex justify-center py-16">
                  {#if view.connectionError}
                    <Alert color="danger" size="small" title="Couldn't load the session">{view.connectionError}</Alert>
                  {:else}
                    <LoadingSpinner size="large" />
                  {/if}
                </div>
              {:else}
                {#if view.transcript.items.length === 0 && session?.phase !== 'starting'}
                  <Text color="muted" class="py-16 text-center">Nothing here yet. Send a message to get going.</Text>
                {/if}
                <TranscriptView items={view.transcript.items} running={view.turnRunning} />
                {#if activity}
                  <div class="text-muted flex items-center gap-2 py-1 text-sm" data-testid="activity">
                    <LoadingSpinner size="tiny" />
                    {activity}
                  </div>
                {/if}
              {/if}
            </div>
          </div>
          {#if !atBottom}
            <div class="absolute end-4 bottom-3">
              <IconButton
                icon={mdiArrowDown}
                aria-label="Scroll to the latest message"
                shape="round"
                color="secondary"
                size="small"
                class="shadow-md"
                onclick={() => scrollToBottom('smooth')}
              />
            </div>
          {/if}
        </div>

        {#if waiting && !sidePanel}
          <div class="border-light-200 max-h-[55dvh] shrink-0 overflow-y-auto border-t px-3 py-2">
            <div class="mx-auto flex w-full max-w-3xl flex-col gap-2">
              {@render waitingItems()}
            </div>
          </div>
        {/if}

        <SessionComposer {view} />
      </div>
      {#if waiting && sidePanel}
        <aside
          class="border-light-200 immich-scrollbar flex w-96 shrink-0 flex-col gap-2 overflow-y-auto border-s p-3"
          aria-label="Waiting on you"
          data-testid="side-panel"
        >
          {@render waitingItems()}
        </aside>
      {/if}
    </div>
  </div>
{/if}

{#if proposalList}
  <ProposalListModal
    proposals={[...(proposalList === 'open' ? openProposals : view.proposals)].reverse()}
    onOpen={(proposalId) => {
      proposalList = null;
      openProposalId = proposalId;
    }}
    onChange={(proposal) => view.upsertProposal(proposal)}
    onClose={() => (proposalList = null)}
  />
{/if}

{#if showGrants && session}
  <GrantsModal grants={session.grants} onClose={() => (showGrants = false)} />
{/if}

{#if showAddRepo && session}
  <AddRepoModal
    {session}
    onAdded={(updated, repo) => {
      view.setSession(updated);
      toastManager.success(`Added ${repo}`);
    }}
    onClose={() => (showAddRepo = false)}
  />
{/if}

{#if openProposal}
  <ProposalModal
    proposal={openProposal}
    onChange={(proposal) => view.upsertProposal(proposal)}
    onClose={() => (openProposalId = null)}
  />
{/if}
