<script lang="ts">
  import {
    Alert,
    Badge,
    Button,
    Field,
    Heading,
    IconButton,
    Input,
    PasswordInput,
    Text,
    Textarea,
    modalManager,
    toastManager,
  } from '@immich/ui';
  import { mdiPlus, mdiTrashCanOutline } from '@mdi/js';
  import { api, type Credential, type GitHubUserConfig } from '#lib/api.ts';
  import { absoluteTime } from '#lib/format.ts';
  import { tokenExpiry, type CredentialKindSpec } from '#lib/presentation.ts';

  let {
    spec,
    credentials,
    onChanged,
  }: { spec: CredentialKindSpec; credentials: Credential[]; onChanged: () => Promise<void> } = $props();

  let adding = $state(false);
  let saving = $state(false);
  let error = $state<string | null>(null);
  let name = $state('');
  let config = $state<Record<string, string>>({});
  let secret = $state('');

  const complete = $derived(
    name.trim() !== '' && secret.trim() !== '' && spec.fields.every((field) => field.optional || (config[field.key] ?? '').trim() !== ''),
  );

  const fieldLabel = (key: string) => spec.fields.find((field) => field.key === key)?.label ?? key;

  function reset() {
    adding = false;
    error = null;
    name = '';
    config = {};
    secret = '';
  }

  function errorMessage(e: unknown) {
    return e instanceof Error ? e.message : String(e);
  }

  async function save(event: SubmitEvent) {
    event.preventDefault();
    if (!complete || saving) return;
    saving = true;
    error = null;
    try {
      const trimmed = Object.fromEntries(
        spec.fields.map((field) => [field.key, (config[field.key] ?? '').trim()]).filter(([, value]) => value !== ''),
      );
      await api.createCredential({ kind: spec.kind, name: name.trim(), config: trimmed, secret: secret.trim() });
      toastManager.success(`Added ${name.trim()}`);
      reset();
      await onChanged();
    } catch (e) {
      error = errorMessage(e);
    } finally {
      saving = false;
    }
  }

  async function makeDefault(credential: Credential) {
    try {
      await api.makeDefaultCredential(credential.id);
      await onChanged();
    } catch (e) {
      toastManager.danger({ title: "Couldn't change the default", description: errorMessage(e) });
    }
  }

  async function remove(credential: Credential) {
    const confirmed = await modalManager.showDialog({
      title: `Delete ${credential.name}?`,
      prompt: `New sessions can no longer use this ${spec.noun}. Its secret is deleted with it.`,
      confirmText: 'Delete',
      icon: mdiTrashCanOutline,
    });
    if (!confirmed) return;
    try {
      await api.deleteCredential(credential.id);
      await onChanged();
    } catch (e) {
      toastManager.danger({ title: `Couldn't delete ${credential.name}`, description: errorMessage(e) });
    }
  }
</script>

<section class="flex flex-col gap-3" data-testid="credentials-{spec.kind}" aria-labelledby="credentials-{spec.kind}-title">
  <div class="flex flex-col gap-1">
    <Heading tag="h2" size="tiny" id="credentials-{spec.kind}-title">{spec.title}</Heading>
    <Text color="muted" size="small">{spec.description}</Text>
  </div>

  {#if credentials.length > 0}
    <ul class="border-light-200 divide-light-200 divide-y rounded-2xl border">
      {#each credentials as credential (credential.id)}
        <li class="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-3" data-testid="credential">
          <div class="flex min-w-0 grow flex-col gap-0.5">
            <div class="flex items-center gap-2">
              <span class="truncate font-medium">{credential.name}</span>
              {#if credential.default}
                <Badge size="tiny" shape="round" color="primary" data-testid="default-badge">Default</Badge>
              {/if}
            </div>
            {#if spec.kind === 'github-user'}
              {@const token = credential.config as GitHubUserConfig}
              {@const expiry = tokenExpiry(token.expires_at)}
              <span class="text-muted truncate text-xs">Account: <span class="font-mono">{token.login}</span></span>
              <span
                class="truncate text-xs {expiry.level === 'ok' ? 'text-muted' : expiry.level === 'soon' ? 'text-warning-700' : 'text-danger'}"
                title={token.expires_at ? absoluteTime(token.expires_at) : undefined}
                data-testid="token-expiry"
                data-level={expiry.level}>{expiry.text}{expiry.level === 'ok' ? '' : '; make a new token and replace this one'}</span
              >
              <span class="text-muted truncate text-xs" title={token.repositories.join('\n')} data-testid="token-repositories">
                Reaches {token.repositories.length}{token.more_repositories ? '+' : ''}
                {token.repositories.length === 1 ? 'repository' : 'repositories'}
              </span>
            {:else}
              {#each Object.entries(credential.config) as [key, value] (key)}
                <span class="text-muted truncate text-xs">{fieldLabel(key)}: <span class="font-mono">{value}</span></span>
              {/each}
            {/if}
          </div>
          <div class="flex items-center gap-1">
            {#if !credential.default}
              <Button size="small" shape="round" variant="outline" color="secondary" onclick={() => makeDefault(credential)}>
                Make default
              </Button>
            {/if}
            <IconButton
              icon={mdiTrashCanOutline}
              aria-label="Delete {credential.name}"
              color="danger"
              variant="ghost"
              shape="round"
              size="small"
              onclick={() => remove(credential)}
            />
          </div>
        </li>
      {/each}
    </ul>
  {:else}
    <Text color="muted" size="small" class="border-light-300 rounded-2xl border border-dashed px-4 py-3">{spec.empty}</Text>
  {/if}

  {#if adding}
    <form class="border-light-200 bg-light-50 flex flex-col gap-3 rounded-2xl border p-4" onsubmit={save}>
      <Heading tag="h3" size="tiny">New {spec.noun}</Heading>
      <Field label="Name" required="indicator">
        <Input bind:value={name} autocomplete="off" />
      </Field>
      {#each spec.fields as field (field.key)}
        <Field label={field.label} description={field.description} required={field.optional ? undefined : 'indicator'}>
          <Input bind:value={config[field.key]} type={field.type ?? 'text'} placeholder={field.placeholder} autocomplete="off" />
        </Field>
      {/each}
      <Field label={spec.secret.label} description={spec.secret.description} required="indicator">
        {#if spec.secret.multiline}
          <Textarea bind:value={secret} rows={6} class="font-mono text-xs" placeholder={spec.secret.placeholder} spellcheck="false" />
        {:else}
          <PasswordInput bind:value={secret} autocomplete="off" />
        {/if}
      </Field>
      <Text color="muted" size="tiny">Secrets are stored by the runtime and never shown again.</Text>
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
        Add {spec.noun}
      </Button>
    </div>
  {/if}
</section>
