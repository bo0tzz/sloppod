<script lang="ts">
  import { IconButton, menuManager, type ActionItem, type MenuItems } from '@immich/ui';
  import { mdiDotsVertical } from '@mdi/js';
  import { tick } from 'svelte';

  /**
   * A ⋮ button opening a menu, like @immich/ui's ContextMenuButton (#367): opened from the keyboard, the menu focuses
   * its first item, so Enter picks it; and an item's action runs once the menu has closed and handed focus back, so a
   * dialog it opens keeps the focus it takes.
   */
  let { items, 'aria-label': label }: { items: MenuItems; 'aria-label': string } = $props();

  async function open(event: MouseEvent) {
    event.stopPropagation();
    event.preventDefault();
    let chosen: (() => void) | null = null;
    const deferred = items.map((item) =>
      typeof item === 'object' && 'onAction' in item
        ? ({ ...item, onAction: (...args) => (chosen = () => item.onAction(...args)) } satisfies ActionItem)
        : item,
    );
    const closed = menuManager.show({ target: event.currentTarget as HTMLElement, position: 'top-right', items: deferred });
    // A click from the keyboard (Enter, Space) has no pointer behind it.
    if (event.detail === 0) {
      await tick();
      requestAnimationFrame(() => document.querySelector<HTMLElement>('[role="menu"] [role="menuitem"]')?.focus());
    }
    await closed;
    await tick();
    (chosen as (() => void) | null)?.();
  }
</script>

<IconButton icon={mdiDotsVertical} color="secondary" variant="ghost" shape="round" aria-label={label} onclick={open} />
