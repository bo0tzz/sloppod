<script module lang="ts">
  /** A repository the new session gets write access to, and who it acts as there: the GitHub App, or one of the user's tokens. */
  export type RepoRow = { id: number; repo: string; credential: string | null };
  /** One of the user's GitHub tokens, with the repositories it reaches (`more`: it reaches others it doesn't list). */
  export type TokenChoice = { id: string; label: string; repositories: string[]; more: boolean };
  export type OwnCapability = { name: string; description: string };
</script>

<script lang="ts">
  import { Button, Checkbox, Field, IconButton, Input, LoadingSpinner, Select, Text } from '@immich/ui';
  import { mdiClose, mdiPlus } from '@mdi/js';

  let {
    title,
    rows,
    capabilities,
    offered,
    own,
    ownChosen,
    tokens,
    repos,
    pending,
    source,
    onTitle,
    onAddRepo,
    onRemoveRepo,
    onRepo,
    onIdentity,
    onCapability,
    onOwn,
  }: {
    title: string;
    rows: RepoRow[];
    /** The built-ins the new session starts with. */
    capabilities: string[];
    /** The built-ins that can be chosen: what the runtime suggests for an empty message. */
    offered: string[];
    own: OwnCapability[];
    ownChosen: string[];
    tokens: TokenChoice[];
    /** What the GitHub App reaches. */
    repos: string[];
    pending: boolean;
    source: 'judgement' | 'match' | null;
    onTitle: (title: string) => void;
    onAddRepo: () => void;
    onRemoveRepo: (id: number) => void;
    onRepo: (id: number, repo: string) => void;
    onIdentity: (id: number, credential: string | null) => void;
    onCapability: (capability: string, on: boolean) => void;
    onOwn: (name: string, on: boolean) => void;
  } = $props();

  const APP = 'app';
  const identities = $derived([
    { value: APP, label: 'The GitHub App' },
    ...tokens.map((token) => ({ value: token.id, label: token.label })),
  ]);
  const tokenOf = (row: RepoRow) => tokens.find((token) => token.id === row.credential);
  /** What a row can choose from: what its identity reaches, less what the other rows already have. */
  function choices(row: RepoRow): string[] {
    const taken = new Set(rows.filter((other) => other.id !== row.id).map((other) => other.repo));
    return (tokenOf(row)?.repositories ?? repos).filter((name) => !taken.has(name));
  }
  const listed = (row: RepoRow) => (tokenOf(row)?.repositories ?? repos).includes(row.repo);
</script>

<div class="bg-subtle flex w-full flex-col gap-3 rounded-2xl p-4 text-left" data-testid="starting-card">
  <div class="flex items-center justify-between gap-2">
    <Text size="small" class="font-medium">Starting set</Text>
    <Text color="muted" size="tiny" data-testid="starting-source">
      {#if pending}
        <span class="inline-flex items-center gap-1"><LoadingSpinner size="tiny" /> Suggesting…</span>
      {:else if source === 'judgement'}
        Suggested by the model
      {:else if source === 'match'}
        Matched from your message
      {/if}
    </Text>
  </div>
  <Field label="Title">
    <Input value={title} placeholder="Taken from the first message" autocomplete="off" oninput={(event) => onTitle(event.currentTarget.value)} />
  </Field>

  <div class="flex flex-col gap-2" data-testid="starting-repos">
    <Text size="small" class="font-medium">Repositories</Text>
    {#each rows as row (row.id)}
      {@const token = tokenOf(row)}
      <div class="flex flex-wrap items-end gap-2" data-testid="starting-repo">
        {#if tokens.length > 0}
          <div class="w-44">
            <Field label="Act as">
              <Select
                options={identities}
                value={row.credential ?? APP}
                onChange={(value) => onIdentity(row.id, value === APP ? null : value)}
              />
            </Field>
          </div>
        {/if}
        <div class="min-w-48 grow">
          <Field label="Repository">
            <Select
              options={choices(row)}
              value={listed(row) ? row.repo : undefined}
              placeholder="Choose a repository"
              onChange={(value) => onRepo(row.id, value)}
            />
          </Field>
        </div>
        {#if token?.more}
          <div class="min-w-40 grow">
            <Field label="Or another">
              <Input
                value={listed(row) ? '' : row.repo}
                placeholder="owner/name"
                autocomplete="off"
                data-testid="starting-unlisted-repo"
                oninput={(event) => onRepo(row.id, event.currentTarget.value.trim())}
              />
            </Field>
          </div>
        {/if}
        <IconButton
          icon={mdiClose}
          aria-label="Remove repository"
          variant="ghost"
          color="secondary"
          shape="round"
          size="small"
          onclick={() => onRemoveRepo(row.id)}
        />
      </div>
      {#if token}
        <Text color="muted" size="tiny">What the session pushes there is attributed to that person.</Text>
      {/if}
    {/each}
    <div>
      <Button size="tiny" shape="round" variant="ghost" leadingIcon={mdiPlus} onclick={onAddRepo}>Add repository</Button>
    </div>
  </div>

  {#if offered.length > 0 || own.length > 0}
    <div class="flex flex-col gap-1">
      <Text size="small" class="font-medium">Access</Text>
      <div class="flex flex-wrap gap-x-4 gap-y-1">
        {#each offered as capability (capability)}
          <label class="flex items-center gap-2 text-sm" data-testid="capability-{capability}">
            <Checkbox
              size="tiny"
              checked={capabilities.includes(capability)}
              aria-label={capability}
              onCheckedChange={(on) => onCapability(capability, on)}
            />
            {capability}
          </label>
        {/each}
        {#each own as definition (definition.name)}
          <label class="flex items-center gap-2 text-sm" data-testid="capability-{definition.name}" title={definition.description}>
            <Checkbox
              size="tiny"
              checked={ownChosen.includes(definition.name)}
              aria-label={definition.name}
              onCheckedChange={(on) => onOwn(definition.name, on)}
            />
            {definition.name}
          </label>
        {/each}
      </div>
    </div>
  {/if}
</div>
