<script lang="ts">
  import { Alert, Button, Field, Heading, IconButton, Input, Select, Text, modalManager, toastManager } from '@immich/ui';
  import { mdiPlus, mdiTrashCanOutline } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api, type Credential, type Definition } from '#lib/api.ts';

  /** The user's `token` credentials, which a definition can attach to its requests. */
  let { tokens }: { tokens: Credential[] } = $props();

  const NONE = '';
  let definitions = $state<Definition[] | null>(null);
  let loadError = $state<string | null>(null);
  let adding = $state(false);
  let saving = $state(false);
  let error = $state<string | null>(null);
  let name = $state('');
  let description = $state('');
  let url = $state('');
  let credential = $state(NONE);

  const complete = $derived(name.trim() !== '' && description.trim() !== '' && url.trim() !== '');
  const credentialOptions = $derived([
    { value: NONE, label: 'None' },
    ...tokens.map((token) => ({ value: token.id, label: token.name })),
  ]);
  const credentialName = (id: string) => tokens.find((token) => token.id === id)?.name ?? 'a credential that no longer exists';
  const errorMessage = (e: unknown) => (e instanceof Error ? e.message : String(e));

  async function refresh() {
    try {
      definitions = await api.listCapabilities();
      loadError = null;
    } catch (e) {
      loadError = errorMessage(e);
    }
  }

  onMount(() => void refresh());

  function reset() {
    adding = false;
    error = null;
    name = '';
    description = '';
    url = '';
    credential = NONE;
  }

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (!complete || saving) return;
    saving = true;
    error = null;
    try {
      await api.createCapability({
        name: name.trim(),
        description: description.trim(),
        mcp_url: url.trim(),
        credential_id: credential === NONE ? undefined : credential,
      });
      toastManager.success(`Added ${name.trim()}`);
      reset();
      await refresh();
    } catch (e) {
      error = errorMessage(e);
    } finally {
      saving = false;
    }
  }

  async function remove(definition: Definition) {
    const confirmed = await modalManager.showDialog({
      title: `Delete ${definition.name}?`,
      prompt: 'Sessions that were granted it keep the grant, but it stops working.',
      confirmText: 'Delete',
      icon: mdiTrashCanOutline,
    });
    if (!confirmed) return;
    try {
      await api.deleteCapability(definition.id);
      await refresh();
    } catch (e) {
      toastManager.danger({ title: `Couldn't delete ${definition.name}`, description: errorMessage(e) });
    }
  }
</script>

<section class="flex flex-col gap-3" data-testid="capabilities" aria-labelledby="capabilities-title">
  <div class="flex flex-col gap-1">
    <Heading tag="h2" size="tiny" id="capabilities-title">Capabilities</Heading>
    <Text color="muted" size="small">
      Your own MCP servers, e.g. your logs or metrics. A session gets one when you grant it by its name from the session's
      Grants; sloppod serves it to the session, so its token never enters the pod.
    </Text>
  </div>

  {#if loadError}
    <Alert color="danger" size="small" title="Couldn't load your capabilities">{loadError}</Alert>
  {:else if definitions && definitions.length > 0}
    <ul class="border-light-200 divide-light-200 divide-y rounded-2xl border">
      {#each definitions as definition (definition.id)}
        <li class="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-3" data-testid="capability">
          <div class="flex min-w-0 grow flex-col gap-0.5">
            <span class="truncate font-mono text-sm font-medium">{definition.name}</span>
            <span class="text-sm">{definition.description}</span>
            <span class="text-muted truncate font-mono text-xs">{definition.mcp_url}</span>
            {#if definition.credential_id}
              <span class="text-muted truncate text-xs">Token: {credentialName(definition.credential_id)}</span>
            {/if}
          </div>
          <IconButton
            icon={mdiTrashCanOutline}
            aria-label="Delete {definition.name}"
            color="danger"
            variant="ghost"
            shape="round"
            size="small"
            onclick={() => remove(definition)}
          />
        </li>
      {/each}
    </ul>
  {:else if definitions}
    <Text color="muted" size="small" class="border-light-300 rounded-2xl border border-dashed px-4 py-3">
      No capability of your own yet.
    </Text>
  {/if}

  {#if adding}
    <form class="border-light-200 bg-light-50 flex flex-col gap-3 rounded-2xl border p-4" onsubmit={save}>
      <Heading tag="h3" size="tiny">New capability</Heading>
      <Field
        label="Name"
        description="Lowercase letters, digits and dashes, starting with a letter; up to 32. It's what you grant it by."
        required="indicator"
      >
        <Input bind:value={name} autocomplete="off" placeholder="victorialogs" />
      </Field>
      <Field label="Description" description="What it's for; the agent sees it with the grant." required="indicator">
        <Input bind:value={description} autocomplete="off" placeholder="Search the cluster's logs" />
      </Field>
      <Field label="MCP server URL" description="An http or https URL, without credentials in it." required="indicator">
        <Input bind:value={url} type="url" autocomplete="off" placeholder="https://logs.example.com/mcp" />
      </Field>
      <Field label="Token" description="One of your token credentials, sent with every request. Add one above.">
        <Select options={credentialOptions} bind:value={credential} />
      </Field>
      {#if error}
        <Alert color="danger" size="small">{error}</Alert>
      {/if}
      <div class="flex justify-end gap-2">
        <Button variant="ghost" color="secondary" shape="round" onclick={reset}>Cancel</Button>
        <Button type="submit" shape="round" loading={saving} disabled={!complete}>Save</Button>
      </div>
    </form>
  {:else}
    <div>
      <Button size="small" shape="round" variant="outline" leadingIcon={mdiPlus} onclick={() => (adding = true)}>
        Add capability
      </Button>
    </div>
  {/if}
</section>
