<script lang="ts">
  import { Button, IconButton, Select, toastManager } from '@immich/ui';
  import { mdiClose, mdiPencilOutline, mdiSend, mdiSteering, mdiStop } from '@mdi/js';
  import { configSelectOptions } from '#lib/presentation.ts';
  import { ConnectionClosedError } from '#lib/acp/connection.ts';
  import { queuedText, type QueuedMessage, type SessionView } from '#lib/session.svelte.ts';
  import { loadDraft, saveDraft } from '#lib/drafts.ts';
  import Composer from './Composer.svelte';

  let { view }: { view: SessionView } = $props();

  // Kept across a page reload, per session. The route remounts this per session, so the first id is the one.
  // svelte-ignore state_referenced_locally
  let text = $state(loadDraft(view.id));
  $effect(() => saveDraft(view.id, text));
  let textarea = $state<HTMLTextAreaElement | null>(null);

  const phase = $derived(view.session?.phase);
  // While the agent is starting or coming back, the runtime queues prompts and runs them as one turn.
  const queued = $derived(phase === 'starting' || phase === 'disconnected');
  // While a turn runs, sending queues the message for the next turn; each queued message can be steered in,
  // edited or removed.
  const working = $derived(!queued && view.turnRunning);
  let steeringId = $state<string | null>(null);
  let removingId = $state<string | null>(null);

  const blockedReason = $derived.by(() => {
    if (phase === 'stopped') return 'This session has stopped.';
    return null;
  });
  const queuedHint = $derived(
    view.connection !== 'open'
      ? "Reconnecting… messages send once it's back."
      : phase === 'starting'
      ? 'The agent is starting; messages are queued.'
      : phase === 'disconnected'
        ? 'The agent is reconnecting; messages are queued.'
        : null,
  );
  const canSend = $derived(!blockedReason && text.trim().length > 0);

  const model = $derived(phase === 'stopped' ? undefined : view.model);
  const modelOptions = $derived(model?.type === 'select' ? configSelectOptions(model.options) : []);

  const unusualStops: Record<string, string> = {
    max_tokens: 'The agent hit its token limit.',
    max_turn_requests: 'The agent hit its limit of model requests for one turn.',
    refusal: 'The agent refused to continue.',
  };

  function errorMessage(error: unknown) {
    return error instanceof Error ? error.message : String(error);
  }

  /** Puts a message back in the box, after any draft there, e.g. one that didn't arrive. */
  function putBack(message: { id: string; text: string }) {
    text = text.trim() ? `${text.trimEnd()}\n\n${message.text}` : message.text;
    dismiss(message.id);
    textarea?.focus();
  }

  function dismiss(id: string) {
    view.undelivered = view.undelivered.filter((message) => message.id !== id);
  }

  /** Sends the message and returns at once: its turn's outcome arrives later, as a toast if it went wrong. */
  function send() {
    const message = text.trim();
    if (!message || blockedReason) return;
    text = '';
    const id = crypto.randomUUID();
    if (view.connection !== 'open') {
      view.sendWhenConnected(message, id);
      textarea?.focus();
      return;
    }
    view.prompt(message, id).then(
      (stopReason) => {
        if (stopReason && stopReason in unusualStops) {
          toastManager.warning({ title: 'The turn ended early', description: unusualStops[stopReason] });
        }
      },
      (error) => {
        // A dropped connection is the session view's to settle once it has reloaded.
        if (error instanceof ConnectionClosedError) return;
        if (!view.received(id) && !text) text = message;
        toastManager.danger({ title: "Couldn't send the prompt", description: errorMessage(error) });
      },
    );
    textarea?.focus();
  }

  async function steer(id: string) {
    steeringId = id;
    try {
      await view.steerQueued(id);
    } catch (error) {
      toastManager.danger({ title: "Couldn't steer the agent", description: errorMessage(error) });
    } finally {
      steeringId = null;
    }
  }

  // The box only holds text, so editing a message with anything else in it would lose that.
  const editable = (message: QueuedMessage) => message.prompt.every((block) => block.type === 'text');

  /** Takes a queued message out of the queue; with `edit`, into the box, after any draft there. */
  async function unqueue(message: QueuedMessage, edit: boolean) {
    removingId = message.id;
    try {
      const prompt = await view.removeQueued(message.id);
      if (edit) putBack({ id: message.id, text: queuedText({ id: message.id, prompt }) });
    } catch (error) {
      // It may have been taken out before the connection dropped: rather than lose it, it's in the box, and if it
      // wasn't taken out it shows in the queue again once the session reloads.
      if (edit && error instanceof ConnectionClosedError) putBack({ id: message.id, text: queuedText(message) });
      else toastManager.danger({ title: "Couldn't take the message back", description: errorMessage(error) });
    } finally {
      removingId = null;
    }
  }

  function editLastQueued() {
    const last = view.queue.at(-1);
    if (last && editable(last) && removingId === null) unqueue(last, true);
  }

  // Esc stops the turn like the Stop button, unless an open dialog, menu or list is the one it should close.
  function onWindowKeydown(event: KeyboardEvent) {
    if (event.key !== 'Escape' || event.defaultPrevented || event.isComposing || !working) return;
    if (document.querySelector('[role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"]')) return;
    event.preventDefault();
    view.cancel();
  }

  async function setModel(value: string) {
    if (model?.type !== 'select' || value === model.currentValue) return;
    try {
      await view.setConfigOption('model', value);
    } catch (error) {
      toastManager.danger({ title: "Couldn't switch the model", description: errorMessage(error) });
    }
  }
</script>

<svelte:window onkeydown={onWindowKeydown} />

{#if view.undelivered.length > 0}
  <div class="border-light-200 shrink-0 border-t px-3 pt-2" data-testid="undelivered-messages">
    <ul class="mx-auto flex w-full max-w-3xl flex-col gap-1">
      {#each view.undelivered as message (message.id)}
        <li class="bg-danger/10 flex items-center gap-2 rounded-xl px-3 py-1.5 text-sm" data-testid="undelivered-message">
          <span class="text-danger shrink-0 text-xs">Didn't arrive</span>
          <span class="min-w-0 grow truncate" title={message.text}>{message.text}</span>
          <Button size="tiny" shape="round" variant="outline" onclick={() => putBack(message)}>Put back</Button>
          <Button size="tiny" shape="round" variant="ghost" color="secondary" onclick={() => dismiss(message.id)}>
            Dismiss
          </Button>
        </li>
      {/each}
    </ul>
  </div>
{/if}

{#if view.unsent.length > 0}
  <div class="border-light-200 shrink-0 border-t px-3 pt-2" data-testid="unsent-messages">
    <ul class="mx-auto flex w-full max-w-3xl flex-col gap-1">
      {#each view.unsent as message (message.id)}
        <li class="bg-light-100 flex items-center gap-2 rounded-xl px-3 py-1.5 text-sm" data-testid="unsent-message">
          <span class="text-muted shrink-0 text-xs">Waiting for the connection</span>
          <span class="min-w-0 grow truncate" title={message.text}>{message.text}</span>
        </li>
      {/each}
    </ul>
  </div>
{/if}

{#if view.queue.length > 0}
  <div class="border-light-200 shrink-0 border-t px-3 pt-2" data-testid="queued-messages">
    <ul class="mx-auto flex w-full max-w-3xl flex-col gap-1">
      {#each view.queue as message (message.id)}
        <li class="bg-light-100 flex items-center gap-2 rounded-xl px-3 py-1.5 text-sm" data-testid="queued-message">
          <span class="text-muted shrink-0 text-xs">Queued</span>
          <span class="min-w-0 grow truncate" title={queuedText(message)}>{queuedText(message)}</span>
          {#if view.steeringSupported}
            <Button
              size="tiny"
              shape="round"
              color="warning"
              variant="outline"
              leadingIcon={mdiSteering}
              loading={steeringId === message.id}
              disabled={steeringId !== null}
              onclick={() => steer(message.id)}
            >
              Steer
            </Button>
          {/if}
          {#if editable(message)}
            <Button
              size="tiny"
              shape="round"
              variant="outline"
              leadingIcon={mdiPencilOutline}
              title="Edit (↑ in an empty box edits the last one)"
              disabled={removingId !== null}
              onclick={() => unqueue(message, true)}
            >
              Edit
            </Button>
          {/if}
          <IconButton
            size="tiny"
            shape="round"
            variant="ghost"
            color="secondary"
            icon={mdiClose}
            aria-label="Remove"
            loading={removingId === message.id}
            disabled={removingId !== null}
            onclick={() => unqueue(message, false)}
          />
        </li>
      {/each}
    </ul>
  </div>
{/if}

<Composer
  bind:text
  bind:ref={textarea}
  label="Prompt"
  placeholder={working ? 'Queue a message for when the agent is done…' : 'Message the agent…'}
  sendLabel="Send"
  sendIcon={mdiSend}
  sendColor="primary"
  {canSend}
  onSend={send}
  onRecall={editLastQueued}
  notice={blockedReason || queuedHint ? notice : undefined}
>
  {#snippet leading()}
    {#if model?.type === 'select' && modelOptions.length > 0}
      <div class="w-32 shrink-0 sm:w-48" data-testid="model-select">
        <Select size="tiny" options={modelOptions} value={model.currentValue} onChange={setModel} title={model.name} />
      </div>
    {/if}
  {/snippet}
  {#snippet trailing()}
    {#if working}
      <Button
        size="small"
        shape="round"
        color="danger"
        variant="outline"
        leadingIcon={mdiStop}
        title="Stop (Esc)"
        onclick={() => view.cancel()}
      >
        Stop
      </Button>
    {/if}
  {/snippet}
</Composer>

{#snippet notice()}
  <span class="text-muted" data-testid="composer-hint">{blockedReason ?? queuedHint}</span>
{/snippet}
