<script lang="ts">
  import { Alert } from '@immich/ui';
  import type { NoticeItem } from '#lib/acp/transcript.svelte.ts';

  let { notice }: { notice: NoticeItem } = $props();

  const color = $derived(notice.severity === 'error' ? 'danger' : notice.severity === 'warning' ? 'warning' : 'info');
</script>

<Alert {color} size="small" title={notice.title}>
  {#if notice.description}{notice.description}{/if}
</Alert>
