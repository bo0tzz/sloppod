<script lang="ts">
  import { Button, Select, toastManager } from '@immich/ui';
  import { mdiSend, mdiSteering, mdiStop } from '@mdi/js';
  import { configSelectOptions } from '#lib/presentation.ts';
  import { ConnectionClosedError } from '#lib/acp/connection.ts';
  import { queuedText, type SessionView } from '#lib/session.svelte.ts';
  import Composer from './Composer.svelte';

  let { view }: { view: SessionView } = $props();

  let text = $state('');
  let sending = $state(false);
  let textarea = $state<HTMLTextAreaElement | null>(null);

  const phase = $derived(view.session?.phase);
  // While the agent is starting or coming back, the runtime queues prompts and runs them as one turn.
  const queued = $derived(phase === 'starting' || phase === 'disconnected');
  // While a turn runs, sending queues the message for the next turn; each queued message can be steered in.
  const working = $derived(!queued && view.turnRunning);
  let steeringId = $state<string | null>(null);

  const blockedReason = $derived.by(() => {
    if (view.connection !== 'open') return 'Reconnecting…';
    if (phase === 'stopped') return 'This session has stopped.';
    return null;
  });
  const queuedHint = $derived(
    phase === 'starting'
      ? 'The agent is starting; messages are queued.'
      : phase === 'disconnected'
        ? 'The agent is reconnecting; messages are queued.'
        : null,
  );
  const canSend = $derived(!blockedReason && text.trim().length > 0 && !sending);

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

  // A message the session view couldn't deliver after a reconnect comes back to the box.
  $effect(() => {
    const message = view.undelivered;
    if (message === null) return;
    view.undelivered = null;
    if (!text) text = message;
    toastManager.danger({
      title: "Your last message didn't arrive",
      description: 'The connection dropped before the runtime got it. It is back in the box to send again.',
    });
  });

  async function send() {
    const message = text.trim();
    if (!message || blockedReason || sending) return;
    sending = true;
    text = '';
    const id = crypto.randomUUID();
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
    sending = false;
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
  {sending}
  onSend={send}
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
