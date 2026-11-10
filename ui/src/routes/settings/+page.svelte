<script lang="ts">
  import { Alert, Button, Heading, LoadingSpinner } from '@immich/ui';
  import { onMount } from 'svelte';
  import { account } from '#lib/account.svelte.ts';
  import { api, type Credential } from '#lib/api.ts';
  import ChatHeader from '#lib/components/ChatHeader.svelte';
  import CapabilitySection from '#lib/components/CapabilitySection.svelte';
  import CredentialSection from '#lib/components/CredentialSection.svelte';
  import SessionDefaults from '#lib/components/SessionDefaults.svelte';
  import TimezoneSetting from '#lib/components/TimezoneSetting.svelte';
  import InstructionsSetting from '#lib/components/InstructionsSetting.svelte';
  import StandingApprovals from '#lib/components/StandingApprovals.svelte';
  import DeletedSessions from '#lib/components/DeletedSessions.svelte';
  import StorageSection from '#lib/components/StorageSection.svelte';
  import { credentialKinds } from '#lib/presentation.ts';

  let credentials = $state<Credential[] | null>(null);
  let error = $state<string | null>(null);
  /** Bumped when credentials change, for the capabilities that name one. */
  let credentialsVersion = $state(0);

  async function refresh() {
    try {
      credentials = await api.listCredentials();
      credentialsVersion++;
      error = null;
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    }
  }

  onMount(() => {
    account.load();
    void refresh();
  });

  /** The sections, grouped, so ten of them in one scroll aren't a page you have to hunt through (#370). */
  const groups = [
    { id: 'defaults', label: 'Defaults' },
    { id: 'credentials', label: 'Credentials' },
    { id: 'capabilities', label: 'Capabilities' },
    { id: 'sessions', label: 'Sessions' },
  ];

  const show = (id: string) => document.getElementById(id)?.scrollIntoView({ block: 'start' });
</script>

<svelte:head>
  <title>Settings · sloppod</title>
</svelte:head>

<div class="flex h-full min-h-0 flex-col">
  <ChatHeader title="Settings" subtitle={account.user ? `Signed in as ${account.user}` : undefined} mono={false} />
  <div class="min-h-0 grow overflow-y-auto">
    <div class="mx-auto flex w-full max-w-2xl flex-col gap-8 px-4 py-6">
      <nav class="flex flex-wrap gap-2" aria-label="Settings sections" data-testid="settings-nav">
        {#each groups as group (group.id)}
          <Button size="tiny" shape="round" variant="ghost" color="secondary" onclick={() => show(group.id)}>
            {group.label}
          </Button>
        {/each}
      </nav>
      <section id="defaults" class="flex flex-col gap-8" data-testid="settings-group-defaults">
        <Heading tag="h2" size="small">Defaults</Heading>
        <SessionDefaults />
        <TimezoneSetting />
        <InstructionsSetting />
        <StandingApprovals />
      </section>
      {#if error}
        <Alert color="danger" size="small" title="Couldn't load credentials">{error}</Alert>
      {/if}
      {#if credentials === null}
        {#if !error}
          <div class="flex justify-center py-12"><LoadingSpinner size="large" /></div>
        {/if}
      {:else}
        <section id="credentials" class="flex flex-col gap-8" data-testid="settings-group-credentials">
          <Heading tag="h2" size="small">Credentials</Heading>
          {#each credentialKinds as spec (spec.kind)}
            <CredentialSection
              {spec}
              credentials={credentials.filter((credential) => credential.kind === spec.kind)}
              onChanged={refresh}
            />
          {/each}
        </section>
        <section id="capabilities" class="flex flex-col gap-8" data-testid="settings-group-capabilities">
          <Heading tag="h2" size="small">Capabilities</Heading>
          <CapabilitySection
            tokens={credentials.filter((credential) => credential.kind === 'token')}
            version={credentialsVersion}
          />
        </section>
      {/if}
      <section id="sessions" class="flex flex-col gap-8" data-testid="settings-group-sessions">
        <Heading tag="h2" size="small">Sessions</Heading>
        <StorageSection />
        <DeletedSessions />
      </section>
    </div>
  </div>
</div>
