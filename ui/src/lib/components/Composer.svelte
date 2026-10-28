<script lang="ts">
  import { Button, Textarea, type Color } from '@immich/ui';
  import type { Snippet } from 'svelte';
  import { MediaQuery } from 'svelte/reactivity';

  let {
    text = $bindable(''),
    ref = $bindable(null),
    label,
    placeholder,
    sendLabel,
    sendIcon,
    sendColor = 'primary',
    canSend,
    sending = false,
    onSend,
    onRecall,
    notice,
    leading,
    trailing,
  }: {
    text?: string;
    ref?: HTMLTextAreaElement | null;
    label: string;
    placeholder: string;
    sendLabel: string;
    sendIcon: string;
    sendColor?: Color;
    canSend: boolean;
    sending?: boolean;
    onSend: () => void;
    /** Up arrow in the empty box, if given. */
    onRecall?: () => void;
    /** Shown on its own line above the controls, on every screen size. */
    notice?: Snippet;
    leading?: Snippet;
    trailing?: Snippet;
  } = $props();

  // On touch keyboards Enter inserts a newline; there's a send button instead.
  const touch = new MediaQuery('(pointer: coarse)');

  // Typing with nothing else focused or selected goes to the message box: focusing it during keydown lets the
  // character land there.
  function onWindowKeydown(event: KeyboardEvent) {
    if (!ref || event.defaultPrevented || event.isComposing || event.ctrlKey || event.metaKey || event.altKey) return;
    // Space is left to scroll the page.
    if (event.key.length !== 1 || event.key === ' ') return;
    const focused = document.activeElement;
    if (focused && focused !== document.body && focused.closest('input, textarea, select, button, a, [contenteditable], [tabindex]')) {
      return;
    }
    if (document.querySelector('[role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"]')) return;
    if (window.getSelection()?.toString()) return;
    ref.focus();
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && (!touch.current || event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      if (canSend && !sending) onSend();
    } else if (onRecall && event.key === 'ArrowUp' && !text && !event.isComposing && !event.shiftKey && !event.altKey && !event.ctrlKey && !event.metaKey) {
      event.preventDefault();
      onRecall();
    }
  }
</script>

<svelte:window onkeydown={onWindowKeydown} />

<div class="safe-bottom border-light-200 bg-light shrink-0 border-t px-3 pt-2" data-testid="composer">
  <div class="mx-auto flex w-full max-w-3xl flex-col gap-2">
    <Textarea
      bind:ref
      bind:value={text}
      grow
      rows={1}
      class="max-h-48"
      {placeholder}
      aria-label={label}
      onkeydown={onKeydown}
    />
    {#if notice}
      <div class="px-1 text-xs" data-testid="composer-notice">{@render notice()}</div>
    {/if}
    <div class="flex items-center gap-2">
      {@render leading?.()}
      <div class="grow"></div>
      {@render trailing?.()}
      <Button
        size="small"
        shape="round"
        color={sendColor}
        leadingIcon={sendIcon}
        disabled={!canSend}
        loading={sending}
        onclick={onSend}
      >
        {sendLabel}
      </Button>
    </div>
  </div>
</div>
