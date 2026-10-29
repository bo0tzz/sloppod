<script lang="ts">
  import { Modal } from '@immich/ui';
  import { onDestroy, type ComponentProps } from 'svelte';
  import { focusInto, opener, returnFocus } from '#lib/dialog-focus.ts';

  /** @immich/ui's Modal, taking focus when it opens and giving it back when it closes (#367). */
  let { children, ...props }: ComponentProps<typeof Modal> = $props();

  const from = opener();
  onDestroy(() => returnFocus(from));

  // The event doesn't say which element is the dialog; the one opening is the topmost, last in the document.
  function onOpenAutoFocus(event: Event) {
    event.preventDefault();
    requestAnimationFrame(() => {
      const dialog = [...document.querySelectorAll<HTMLElement>('[role="dialog"]')].at(-1);
      if (dialog) focusInto(dialog);
    });
  }
</script>

<Modal {...props} {onOpenAutoFocus}>
  {@render children()}
</Modal>
