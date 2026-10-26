<script lang="ts">
  import { renderMarkdown } from '#lib/markdown.ts';

  let { text, breaks = false, class: className = '' }: { text: string; breaks?: boolean; class?: string } = $props();

  const html = $derived(renderMarkdown(text, { breaks }));
</script>

<div class="markdown {className}">
  {@html html}
</div>
