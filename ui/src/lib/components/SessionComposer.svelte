<script lang="ts">
  import { Button, Select, toastManager } from '@immich/ui';
  import { mdiSend, mdiSteering, mdiStop } from '@mdi/js';
  import { configSelectOptions } from '#lib/presentation.ts';
  import type { SessionView } from '#lib/session.svelte.ts';
  import Composer from './Composer.svelte';

  let { view }: { view: SessionView } = $props();

  let text = $state('');
  let sending = $state(false);
  let textarea = $state<HTMLTextAreaElement | null>(null);

  const phase = $derived(view.session?.phase);
  // While the agent is starting or coming back, the runtime queues prompts and runs them as one turn.
  const queued = $derived(phase === 'starting' || phase === 'disconnected');
  const steering = $derived(!queued && view.turnRunning);

  const blockedReason = $derived.by(() => {
    if (view.connection !== 'open') return 'Reconnecting…';
    if (phase === 'stopped') return 'This session has stopped.';
    if (steering && !view.steeringSupported) return "The agent can't take messages mid-turn.";
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

  async function send() {
    const message = text.trim();
    if (!message || blockedReason || sending) return;
    sending = true;
    try {
      if (steering) {
        await view.steer(message);
        text = '';
      } else {
        text = '';
        view.prompt(message).then(
          (stopReason) => {
            if (stopReason && stopReason in unusualStops) {
              toastManager.warning({ title: 'The turn ended early', description: unusualStops[stopReason] });
            }
          },
          (error) => {
            // A dropped socket doesn't end the turn; the reload shows what happened.
            if (view.connection !== 'open') return;
            if (!text) text = message;
            toastManager.danger({ title: "Couldn't send the prompt", description: errorMessage(error) });
          },
        );
      }
    } catch (error) {
      toastManager.danger({ title: "Couldn't steer the agent", description: errorMessage(error) });
    } finally {
      sending = false;
      textarea?.focus();
    }
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

<Composer
  bind:text
  bind:ref={textarea}
  label={steering ? 'Steering message' : 'Prompt'}
  placeholder={steering ? 'Steer the agent…' : 'Message the agent…'}
  sendLabel={steering ? 'Steer' : 'Send'}
  sendIcon={steering ? mdiSteering : mdiSend}
  sendColor={steering ? 'warning' : 'primary'}
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
    {#if steering}
      <Button size="small" shape="round" color="danger" variant="outline" leadingIcon={mdiStop} onclick={() => view.cancel()}>
        Stop
      </Button>
    {/if}
  {/snippet}
</Composer>

{#snippet notice()}
  <span class="text-muted" data-testid="composer-hint">{blockedReason ?? queuedHint}</span>
{/snippet}
