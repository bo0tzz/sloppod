<script lang="ts">
  import { goto } from '$app/navigation';
  import { Alert, Button, Field, Heading, IconButton, Input, Select, Textarea } from '@immich/ui';
  import { mdiArrowLeft, mdiRocketLaunchOutline } from '@mdi/js';
  import { onMount } from 'svelte';
  import { api } from '#lib/api.ts';
  import { sessions } from '#lib/sessions.svelte.ts';

  let repos = $state<string[] | null>(null);
  let repo = $state<string>();
  let title = $state('');
  let prompt = $state('');
  let submitting = $state(false);
  let error = $state<string | null>(null);

  onMount(async () => {
    try {
      repos = await api.listRepos();
      if (repos.length === 1) repo = repos[0];
    } catch (e) {
      repos = [];
      error = `Couldn't load repositories: ${e instanceof Error ? e.message : e}`;
    }
  });

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!repo || submitting) return;
    submitting = true;
    error = null;
    try {
      const session = await api.createSession({
        repo,
        title: title.trim() || undefined,
        prompt: prompt.trim() || undefined,
      });
      sessions.upsert(session);
      await goto(`/sessions/${session.id}`);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      submitting = false;
    }
  }

  function onPromptKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) {
      (event.currentTarget as HTMLTextAreaElement).form?.requestSubmit();
    }
  }
</script>

<svelte:head>
  <title>New session · sloppod</title>
</svelte:head>

<div class="mx-auto flex w-full max-w-2xl flex-col gap-6 px-4 pt-4 pb-8">
  <header class="flex items-center gap-2">
    <IconButton href="/" icon={mdiArrowLeft} aria-label="Back to sessions" variant="ghost" shape="round" color="secondary" class="md:hidden" />
    <Heading tag="h1" size="small">New session</Heading>
  </header>

  <form class="flex flex-col gap-5" onsubmit={submit} data-testid="new-session-form">
    <Field label="Repository" required="indicator">
      <Select
        options={repos ?? []}
        bind:value={repo}
        placeholder={repos === null ? 'Loading…' : repos.length ? 'Choose a repository' : 'No repositories available'}
      />
    </Field>

    <Field label="Title" description="Optional">
      <Input bind:value={title} placeholder="e.g. Fix the flaky reconnect test" name="title" />
    </Field>

    <Field label="Prompt" description="Sent to the agent as soon as it's up.">
      <Textarea bind:value={prompt} rows={8} name="prompt" placeholder="What should the agent do?" onkeydown={onPromptKeydown} />
    </Field>

    {#if error}
      <Alert color="danger" size="small" title="Couldn't start the session">{error}</Alert>
    {/if}

    <div class="flex justify-end">
      <Button type="submit" disabled={!repo} loading={submitting} leadingIcon={mdiRocketLaunchOutline} shape="round">
        Start session
      </Button>
    </div>
  </form>
</div>
