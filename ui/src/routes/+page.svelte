<script lang="ts">
  import { Alert, Button, Heading, LoadingSpinner } from '@immich/ui';
  import { mdiPlus } from '@mdi/js';
  import AppIcon from '#lib/components/AppIcon.svelte';
  import SessionList from '#lib/components/SessionList.svelte';
  import { sessions } from '#lib/sessions.svelte.ts';
</script>

<svelte:head>
  <title>Sessions · sloppod</title>
</svelte:head>

<div class="mx-auto flex w-full max-w-3xl flex-col gap-4 px-4 pt-4 pb-8">
  <header class="flex items-center gap-3">
    <AppIcon class="size-8" />
    <Heading tag="h1" size="small" class="grow">Sessions</Heading>
    <Button href="/new" leadingIcon={mdiPlus} shape="round" size="small">New session</Button>
  </header>

  {#if sessions.error}
    <Alert color="danger" title="Couldn't load sessions" size="small">{sessions.error}</Alert>
  {/if}

  {#if !sessions.loaded}
    <div class="flex justify-center py-12"><LoadingSpinner size="large" /></div>
  {:else}
    <SessionList sessions={sessions.sorted} />
  {/if}
</div>
