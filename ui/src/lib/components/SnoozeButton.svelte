<script lang="ts">
  import { Button, menuManager, type ActionItem } from '@immich/ui';
  import { mdiClockOutline } from '@mdi/js';
  import { tick } from 'svelte';
  import { snoozeChoices } from '#lib/snooze.ts';
  import { userTimezone } from '#lib/user-timezone.svelte.ts';

  /**
   * Snooze, next to Seen (#467): a menu of when to be shown it again, worked out in the user's timezone. The choice
   * comes back before the action runs, so a second one can't land while the first is being saved.
   */
  let { onSnooze, loading = false }: { onSnooze: (until: Date) => Promise<void>; loading?: boolean } = $props();

  async function open(event: MouseEvent) {
    event.stopPropagation();
    event.preventDefault();
    const target = event.currentTarget as HTMLElement;
    // Read as the menu opens, so a zone changed in Settings since it was last read (#482) is the one used.
    let chosen: Date | null = null;
    const items: ActionItem[] = snoozeChoices(userTimezone.value).map((choice) => ({
      title: choice.title,
      onAction: () => {
        chosen = choice.until;
      },
    }));
    const closed = menuManager.show({ target, position: 'top-right', items });
    // A click from the keyboard (Enter, Space) has no pointer behind it.
    if (event.detail === 0) {
      await tick();
      requestAnimationFrame(() => document.querySelector<HTMLElement>('[role="menu"] [role="menuitem"]')?.focus());
    }
    await closed;
    await tick();
    const until: Date | null = chosen;
    if (until) await onSnooze(until);
  }
</script>

<Button size="small" shape="round" variant="ghost" leadingIcon={mdiClockOutline} loading={loading} onclick={open}>
  Snooze
</Button>
