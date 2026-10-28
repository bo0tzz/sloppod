<script lang="ts">
  import { Alert, LoadingSpinner } from '@immich/ui';
  import { onMount } from 'svelte';
  import { account } from '#lib/account.svelte.ts';
  import { api, type Credential } from '#lib/api.ts';
  import ChatHeader from '#lib/components/ChatHeader.svelte';
  import CredentialSection from '#lib/components/CredentialSection.svelte';
  import SessionDefaults from '#lib/components/SessionDefaults.svelte';
  import TimezoneSetting from '#lib/components/TimezoneSetting.svelte';
  import StandingApprovals from '#lib/components/StandingApprovals.svelte';
  import DeletedSessions from '#lib/components/DeletedSessions.svelte';
  import { credentialKinds } from '#lib/presentation.ts';

  let credentials = $state<Credential[] | null>(null);
  let error = $state<string | null>(null);

  async function refresh() {
    try {
      credentials = await api.listCredentials();
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  onMount(() => {
    account.load();
    void refresh();
  });
</script>

<svelte:head>
  <title>Settings · sloppod</title>
</svelte:head>

<div class="flex h-full min-h-0 flex-col">
  <ChatHeader title="Settings" subtitle={account.user ? `Signed in as ${account.user}` : undefined} mono={false} />
  <div class="min-h-0 grow overflow-y-auto">
    <div class="mx-auto flex w-full max-w-2xl flex-col gap-8 px-4 py-6">
      <SessionDefaults />
      <TimezoneSetting />
      <StandingApprovals />
      <DeletedSessions />
      {#if error}
        <Alert color="danger" size="small" title="Couldn't load credentials">{error}</Alert>
      {/if}
      {#if credentials === null}
        {#if !error}
          <div class="flex justify-center py-12"><LoadingSpinner size="large" /></div>
        {/if}
      {:else}
        {#each credentialKinds as spec (spec.kind)}
          <CredentialSection
            {spec}
            credentials={credentials.filter((credential) => credential.kind === spec.kind)}
            onChanged={refresh}
          />
        {/each}
      {/if}
    </div>
  </div>
</div>
