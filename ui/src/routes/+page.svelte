<script lang="ts">
  import { Alert, Button, Heading, IconButton, LoadingSpinner } from '@immich/ui';
  import { mdiCogOutline, mdiFileTreeOutline, mdiPlus } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api } from '#lib/api.ts';
  import AppIcon from '#lib/components/AppIcon.svelte';
  import QuotaMeters from '#lib/components/QuotaMeters.svelte';
  import SessionList from '#lib/components/SessionList.svelte';
  import WorkFrontier from '#lib/components/WorkFrontier.svelte';
  import { sessions } from '#lib/sessions.svelte.ts';

  let noClaude = $state(false);
  let noRepos = $state(false);

  onMount(async () => {
    const [credentials, repos] = await Promise.allSettled([api.listCredentials(), api.listRepos()]);
    noClaude = credentials.status === 'fulfilled' && !credentials.value.some((credential) => credential.kind === 'claude');
    noRepos = repos.status === 'fulfilled' && repos.value.length === 0;
  });
</script>

<svelte:head>
  <title>Sessions · sloppod</title>
</svelte:head>

<div class="mx-auto flex w-full max-w-3xl flex-col gap-4 px-4 pt-4 pb-8">
  <header class="flex items-center gap-3">
    <AppIcon class="size-8" />
    <Heading tag="h1" size="small" class="grow">Sessions</Heading>
    <Button href="/new" leadingIcon={mdiPlus} shape="round" size="small">New session</Button>
    <IconButton href="/work" icon={mdiFileTreeOutline} aria-label="Work" variant="ghost" shape="round" color="secondary" />
    <IconButton href="/settings" icon={mdiCogOutline} aria-label="Settings" variant="ghost" shape="round" color="secondary" />
  </header>

  {#if noClaude}
    <div data-testid="nudge-claude">
      <Alert color="warning" size="small" title="No Claude account">
        Sessions need one to start. <a href="/settings" class="underline">Add it in settings</a>.
      </Alert>
    </div>
  {/if}
  {#if noRepos}
    <div data-testid="nudge-repos">
      <Alert color="info" size="small" title="No repositories available">
        Sessions can still chat, but can't work on code. <a href="/settings" class="underline">Add a GitHub App in settings</a>.
      </Alert>
    </div>
  {/if}

  <!-- The sidebar, where they are elsewhere, isn't on this page, and on a phone it isn't anywhere. -->
  <QuotaMeters framed />

  <WorkFrontier />

  {#if sessions.error}
    <Alert color="danger" title="Couldn't load sessions" size="small">{sessions.error}</Alert>
  {/if}

  {#if !sessions.loaded}
    <div class="flex justify-center py-12"><LoadingSpinner size="large" /></div>
  {:else}
    <SessionList sessions={sessions.sorted} />
  {/if}
</div>
