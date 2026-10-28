<script lang="ts">
  import { Button, IconButton, Select, toastManager } from '@immich/ui';
  import { mdiClose, mdiImagePlus, mdiSend, mdiSteering, mdiStop } from '@mdi/js';
  import { configSelectOptions } from '#lib/presentation.ts';
  import { ConnectionClosedError } from '#lib/acp/connection.ts';
  import { queuedText, type SessionView } from '#lib/session.svelte.ts';
  import { loadDraft, saveDraft, type Outgoing } from '#lib/drafts.ts';
  import { attach, attachmentSrc, MAX_IMAGES, type Attachment } from '#lib/images.ts';
  import Composer from './Composer.svelte';

  let { view }: { view: SessionView } = $props();

  // Kept across a page reload, per session. The route remounts this per session, so the first id is the one.
  // svelte-ignore state_referenced_locally
  let text = $state(loadDraft(view.id));
  $effect(() => saveDraft(view.id, text));
  let textarea = $state<HTMLTextAreaElement | null>(null);
  // Not kept across a reload: they're only in the box until sent.
  let images = $state<Attachment[]>([]);
  let attaching = $state(0);
  let picker = $state<HTMLInputElement | null>(null);

  const phase = $derived(view.session?.phase);
  // While the agent is starting or coming back, the runtime queues prompts and runs them as one turn.
  const queued = $derived(phase === 'starting' || phase === 'disconnected');
  // While a turn runs, sending queues the message for the next turn; each queued message can be steered in.
  const working = $derived(!queued && view.turnRunning);
  let steeringId = $state<string | null>(null);

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
  const canSend = $derived(!blockedReason && attaching === 0 && (text.trim().length > 0 || images.length > 0));

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

  /** Puts a message that didn't arrive back in the box, after any draft there. */
  function putBack(message: Outgoing) {
    if (message.text) text = text.trim() ? `${text.trimEnd()}\n\n${message.text}` : message.text;
    images = [...images, ...(message.images ?? [])];
    dismiss(message.id);
    textarea?.focus();
  }

  function dismiss(id: string) {
    view.undelivered = view.undelivered.filter((message) => message.id !== id);
  }

  /** What a message says, for the one-line lists of messages waiting or not delivered. */
  function outline(message: Outgoing) {
    const count = message.images?.length ?? 0;
    const pictures = count === 1 ? 'an image' : `${count} images`;
    if (!count) return message.text;
    return message.text ? `${message.text} (with ${pictures})` : `(${pictures})`;
  }

  async function addImages(files: File[]) {
    const room = MAX_IMAGES - images.length;
    if (files.length > room) {
      toastManager.warning({ title: `A message can carry up to ${MAX_IMAGES} images` });
    }
    attaching++;
    try {
      for (const file of files.slice(0, Math.max(0, room))) {
        try {
          images = [...images, await attach(file)];
        } catch (error) {
          toastManager.danger({ title: "Couldn't add the image", description: errorMessage(error) });
        }
      }
    } finally {
      attaching--;
    }
    textarea?.focus();
  }

  function picked(event: Event) {
    const input = event.currentTarget as HTMLInputElement;
    void addImages([...(input.files ?? [])]);
    input.value = '';
  }

  /** Sends the message and returns at once: its turn's outcome arrives later, as a toast if it went wrong. */
  function send() {
    const content = text.trim();
    const sent = images;
    if (!canSend) return;
    text = '';
    images = [];
    const id = crypto.randomUUID();
    const message: Outgoing = { id, text: content, ...(sent.length ? { images: sent } : {}) };
    if (view.connection !== 'open') {
      view.sendWhenConnected(message);
      textarea?.focus();
      return;
    }
    view.prompt(message).then(
      (stopReason) => {
        if (stopReason && stopReason in unusualStops) {
          toastManager.warning({ title: 'The turn ended early', description: unusualStops[stopReason] });
        }
      },
      (error) => {
        // A dropped connection is the session view's to settle once it has reloaded.
        if (error instanceof ConnectionClosedError) return;
        if (!view.received(id) && !text && images.length === 0) {
          text = content;
          images = sent;
        }
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
          <span class="min-w-0 grow truncate" title={outline(message)}>{outline(message)}</span>
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
          <span class="min-w-0 grow truncate" title={outline(message)}>{outline(message)}</span>
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
  onFiles={view.imagesSupported ? addImages : undefined}
  attachments={images.length > 0 || attaching > 0 ? attachments : undefined}
  notice={blockedReason || queuedHint ? notice : undefined}
>
  {#snippet leading()}
    {#if view.imagesSupported}
      <input bind:this={picker} type="file" accept="image/*" multiple hidden onchange={picked} data-testid="image-picker" />
      <IconButton
        size="small"
        shape="round"
        variant="ghost"
        color="secondary"
        icon={mdiImagePlus}
        aria-label="Add images"
        title="Add images (or paste or drop them)"
        disabled={images.length >= MAX_IMAGES}
        onclick={() => picker?.click()}
      />
    {/if}
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

{#snippet attachments()}
  <ul class="flex flex-wrap gap-2" data-testid="attachments">
    {#each images as image, index (index)}
      <li class="border-light-200 relative size-16 overflow-hidden rounded-lg border" data-testid="attachment">
        <img src={attachmentSrc(image)} alt="Attached image {index + 1}" class="size-full object-cover" />
        <IconButton
          size="tiny"
          shape="round"
          color="secondary"
          icon={mdiClose}
          aria-label="Remove image {index + 1}"
          class="absolute top-0.5 right-0.5"
          onclick={() => (images = images.filter((_, i) => i !== index))}
        />
      </li>
    {/each}
    {#if attaching > 0}
      <li class="border-light-200 text-muted flex size-16 items-center justify-center rounded-lg border text-xs">Adding…</li>
    {/if}
  </ul>
{/snippet}
