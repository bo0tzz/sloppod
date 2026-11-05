<script lang="ts">
  import { Alert, Button } from '@immich/ui';
  import type { NoticeItem } from '#lib/acp/transcript.svelte.ts';

  /** `onResume`: shown on a stopped or failed turn's marker while nothing has come since (#406). */
  let { notice, onResume }: { notice: NoticeItem; onResume?: () => void } = $props();

  const color = $derived(notice.severity === 'error' ? 'danger' : notice.severity === 'warning' ? 'warning' : 'info');
</script>

<div data-testid="notice">
  <Alert {color} size="small" title={notice.title}>
    {#if notice.description}{notice.description}{/if}
    {#if onResume}
      <div class="mt-2">
        <Button size="small" shape="round" variant="outline" color="secondary" onclick={onResume} data-testid="resume-turn">
          Resume
        </Button>
      </div>
    {/if}
  </Alert>
</div>
