<script lang="ts">
  import { Button, Select, Textarea, toastManager } from '@immich/ui';
  import { mdiSend, mdiSteering, mdiStop } from '@mdi/js';
  import { MediaQuery } from 'svelte/reactivity';
  import type { SessionView } from '#lib/session.svelte.ts';

  let { view }: { view: SessionView } = $props();

  let text = $state('');
  let sending = $state(false);
  let textarea = $state<HTMLTextAreaElement | null>(null);

  // On touch keyboards Enter inserts a newline; there's a send button instead.
  const touch = new MediaQuery('(pointer: coarse)');

  const phase = $derived(view.session?.phase);
  const running = $derived(view.turnRunning);
  const blockedReason = $derived.by(() => {
    if (view.connection !== 'open') return 'Reconnecting…';
    if (phase === 'stopped') return 'This session has stopped.';
    if (phase === 'starting') return 'Waiting for the agent to start…';
    if (phase === 'disconnected') return 'The agent is reconnecting…';
    if (running && !view.steeringSupported) return "The agent can't take messages mid-turn.";
    return null;
  });
  const canSend = $derived(!blockedReason && text.trim().length > 0 && !sending);

  const modeOptions = $derived(
    phase === 'stopped' ? [] : view.modes.map((mode) => ({ value: mode.id, label: mode.name })),
  );

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
      if (running) {
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

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && (!touch.current || event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      void send();
    }
  }

  async function setMode(modeId: string) {
    if (modeId === view.currentModeId) return;
    try {
      await view.setMode(modeId);
    } catch (error) {
      toastManager.danger({ title: "Couldn't switch mode", description: errorMessage(error) });
    }
  }
</script>

<div class="safe-bottom border-light-200 bg-light shrink-0 border-t px-3 pt-2" data-testid="composer">
  <div class="mx-auto flex w-full max-w-3xl flex-col gap-2">
    <Textarea
      bind:ref={textarea}
      bind:value={text}
      grow
      rows={1}
      class="max-h-48"
      placeholder={running ? 'Steer the agent…' : 'Message the agent…'}
      aria-label={running ? 'Steering message' : 'Prompt'}
      onkeydown={onKeydown}
    />
    {#if blockedReason}
      <span class="text-muted px-1 text-xs" data-testid="composer-hint">{blockedReason}</span>
    {/if}
    <div class="flex items-center gap-2">
      {#if modeOptions.length > 0}
        <div class="w-36 shrink-0 sm:w-44" data-testid="mode-select">
          <Select
            size="tiny"
            options={modeOptions}
            value={view.currentModeId ?? undefined}
            onChange={setMode}
            title="Permission mode"
          />
        </div>
      {/if}
      <span class="text-muted min-w-0 grow truncate text-xs">
        <span class="hidden sm:inline">{running && !blockedReason ? 'Messages now steer the running turn.' : ''}</span>
      </span>
      {#if running}
        <Button size="small" shape="round" color="danger" variant="outline" leadingIcon={mdiStop} onclick={() => view.cancel()}>
          Stop
        </Button>
      {/if}
      <Button
        size="small"
        shape="round"
        color={running ? 'warning' : 'primary'}
        leadingIcon={running ? mdiSteering : mdiSend}
        disabled={!canSend}
        loading={sending}
        onclick={send}
      >
        {running ? 'Steer' : 'Send'}
      </Button>
    </div>
  </div>
</div>
