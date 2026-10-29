<script lang="ts">
  import { Button, Icon } from '@immich/ui';
  import { mdiChatQuestionOutline, mdiShieldAlertOutline, mdiSourcePull } from '@mdi/js';
  import { claimsFocus } from '#lib/focus.ts';

  /** The waiting panel folded to one line (#374): what waits on the user, and a button that unfolds it. */
  let {
    kind,
    text,
    onUnfold,
  }: { kind: 'question' | 'permission' | 'proposal'; text: string; onUnfold: () => void } = $props();

  const icons = { question: mdiChatQuestionOutline, permission: mdiShieldAlertOutline, proposal: mdiSourcePull };
  const actions = { question: 'Answer', permission: 'Review', proposal: 'Show' };
</script>

{#snippet bar()}
  <Icon icon={icons[kind]} class="text-muted shrink-0" size="1.25em" aria-hidden />
  <span class="min-w-0 grow truncate text-sm" data-testid="waiting-summary-text">{text}</span>
  <Button size="small" shape="round" onclick={onUnfold}>{actions[kind]}</Button>
{/snippet}

<!-- A question folded away still takes focus, as its card would. -->
{#if kind === 'question'}
  <div class="flex items-center gap-2 outline-none" data-testid="waiting-summary" tabindex="-1" use:claimsFocus>
    {@render bar()}
  </div>
{:else}
  <div class="flex items-center gap-2" data-testid="waiting-summary">
    {@render bar()}
  </div>
{/if}
