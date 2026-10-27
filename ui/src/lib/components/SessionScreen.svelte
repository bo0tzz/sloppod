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
  import { mdiArrowDown, mdiCogOutline, mdiSourcePull, mdiTrashCanOutline } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api } from '#lib/api.ts';
  import { SessionView } from '#lib/session.svelte.ts';
  import { sessions } from '#lib/sessions.svelte.ts';
  import ChatHeader from './ChatHeader.svelte';
  import PermissionCard from './PermissionCard.svelte';
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
  const showReconnecting = $derived(view.loaded && view.connection !== 'open');
  const activity = $derived.by(() => {
    switch (session?.phase) {
      case 'starting':
        return 'Starting the agent…';
      case 'disconnected':
        return 'Bringing the agent back…';
      case 'waiting':
        return 'Waiting for your decision';
      default:
        return view.turnRunning ? 'Working…' : null;
    }
  });

  let openProposalId = $state<string | null>(null);
  let showProposalList = $state(false);

  let scroller = $state<HTMLElement | null>(null);
  let content = $state<HTMLElement | null>(null);
  let atBottom = $state(true);

  onMount(() => {
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

  async function deleteSession() {
    const confirmed = await modalManager.showDialog({
      title: 'Delete session?',
      prompt: 'This stops the session and deletes its pod and volume.',
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
          onclick={() => (showProposalList = true)}
        />
        <ContextMenuButton
          aria-label="Session actions"
          items={[
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

    {#if view.permissions.length > 0 || pendingProposals.length > 0}
      <div class="border-light-200 max-h-[55dvh] shrink-0 overflow-y-auto border-t px-3 py-2">
        <div class="mx-auto flex w-full max-w-3xl flex-col gap-2">
          {#each view.permissions as request (request.rpcId)}
            <PermissionCard
              {request}
              tool={view.transcript.tool(request.toolCall.toolCallId)}
              onAnswer={(optionId) => view.answerPermission(request, optionId)}
            />
          {/each}
          {#each pendingProposals as proposal (proposal.id)}
            <ProposalCard {proposal} onOpen={() => (openProposalId = proposal.id)} />
          {/each}
        </div>
      </div>
    {/if}

    <SessionComposer {view} />
  </div>
{/if}

{#if showProposalList}
  <ProposalListModal
    proposals={[...view.proposals].reverse()}
    onOpen={(proposalId) => {
      showProposalList = false;
      openProposalId = proposalId;
    }}
    onClose={() => (showProposalList = false)}
  />
{/if}

{#if openProposalId}
  <ProposalModal
    id={openProposalId}
    onChange={(proposal) => view.upsertProposal(proposal)}
    onClose={() => (openProposalId = null)}
  />
{/if}
