<script lang="ts">
  import { Badge, Button, Icon, Input, NumberInput, Select } from '@immich/ui';
  import { mdiChatQuestionOutline } from '@mdi/js';
  import type { ElicitationField, ElicitationResponse, EnumOption } from '#lib/acp/types.ts';
  import type { PendingQuestion } from '#lib/session.svelte.ts';
  import Markdown from './Markdown.svelte';

  let { request, onRespond }: { request: PendingQuestion; onRespond: (response: ElicitationResponse) => void } =
    $props();

  const PREVIEW_KEY = '_claude/askUserQuestionOption';

  const fields = $derived(Object.entries(request.requestedSchema?.properties ?? {}));
  const required = $derived(new Set(request.requestedSchema?.required ?? []));

  // svelte-ignore state_referenced_locally
  let values = $state<Record<string, unknown>>(
    Object.fromEntries(
      Object.entries(request.requestedSchema?.properties ?? {})
        .filter(([, field]) => field.default !== undefined)
        .map(([key, field]) => [key, field.default]),
    ),
  );
  /** The option whose preview is shown, per field: the one hovered or focused, else the one picked. */
  let previewing = $state<Record<string, string | undefined>>({});

  type Kind = 'single' | 'multi' | 'select' | 'text' | 'number' | 'boolean' | 'other';

  function kindOf(key: string, field: ElicitationField): Kind {
    if (key.endsWith('_custom') && request.requestedSchema.properties[key.slice(0, -'_custom'.length)]) return 'other';
    if (field.type === 'string' && field.oneOf?.length) return 'single';
    if (field.type === 'string' && field.enum?.length) return 'select';
    if (field.type === 'array') return 'multi';
    if (field.type === 'number' || field.type === 'integer') return 'number';
    if (field.type === 'boolean') return 'boolean';
    return 'text';
  }

  const multiOptions = (field: ElicitationField): EnumOption[] =>
    field.items?.anyOf ?? (field.items?.enum ?? []).map((value) => ({ const: value, title: value }));

  const preview = (option: EnumOption | undefined) =>
    (option?._meta?.[PREVIEW_KEY] as { preview?: string } | undefined)?.preview;

  function shownPreview(key: string, options: EnumOption[]) {
    const value = values[key];
    const picked = Array.isArray(value) ? value.at(-1) : value;
    const target = previewing[key] ?? picked;
    return preview(options.find((option) => option.const === target));
  }

  function toggle(key: string, value: string, multi: boolean) {
    if (multi) {
      const current = Array.isArray(values[key]) ? (values[key] as string[]) : [];
      values[key] = current.includes(value) ? current.filter((item) => item !== value) : [...current, value];
    } else {
      // Nothing is required, so picking the chosen option again clears it.
      values[key] = values[key] === value ? undefined : value;
    }
  }

  /** The form's content: picked consts, arrays of them in the schema's order, and typed text; empty fields left out. */
  function content(): Record<string, unknown> {
    const result: Record<string, unknown> = {};
    for (const [key, field] of fields) {
      const value = values[key];
      if (value === undefined || value === null) continue;
      if (Array.isArray(value)) {
        const order = multiOptions(field).map((option) => option.const);
        if (value.length) result[key] = [...value].sort((a, b) => order.indexOf(a) - order.indexOf(b));
      } else if (typeof value === 'string') {
        if (value.trim()) result[key] = kindOf(key, field) === 'single' || field.enum ? value : value.trim();
      } else if (typeof value === 'number') {
        if (Number.isFinite(value)) result[key] = field.type === 'integer' ? Math.trunc(value) : value;
      } else {
        result[key] = value;
      }
    }
    return result;
  }

  const missingRequired = $derived(
    [...required].some((key) => {
      const value = values[key];
      return value === undefined || value === '' || (Array.isArray(value) && value.length === 0);
    }),
  );

  const answer = () => onRespond({ action: 'accept', content: content() });

  function submitOnEnter(event: KeyboardEvent) {
    if (event.key !== 'Enter' || event.shiftKey || event.isComposing) return;
    event.preventDefault();
    if (!missingRequired) answer();
  }
</script>

<section
  class="border-info-300 bg-info-50 dark:bg-info-100/40 flex flex-col gap-3 rounded-2xl border p-3"
  data-testid="question-card"
  aria-label="Question from the agent"
>
  <header class="flex items-start gap-2 text-sm">
    <Icon icon={mdiChatQuestionOutline} class="text-info-700 mt-0.5 shrink-0" size="1.25em" aria-hidden />
    <Markdown text={request.message} breaks class="min-w-0 font-semibold" />
  </header>

  {#each fields as [key, field] (key)}
    {@const kind = kindOf(key, field)}
    <div class="flex flex-col gap-1.5 {kind === 'other' ? '-mt-1.5' : ''}" data-testid="question-field" data-key={key}>
      {#if kind !== 'other' && kind !== 'boolean' && (field.title || field.description)}
        <div class="flex flex-wrap items-center gap-2">
          {#if field.title}
            <Badge size="tiny" shape="round" color="info">{field.title}</Badge>
          {/if}
          {#if field.description}
            <span class="text-sm font-medium">{field.description}</span>
          {/if}
        </div>
      {/if}

      {#if kind === 'single' || kind === 'multi'}
        {@const options = kind === 'single' ? (field.oneOf ?? []) : multiOptions(field)}
        {@const shown = shownPreview(key, options)}
        <div class="flex flex-col gap-1" role={kind === 'single' ? 'radiogroup' : 'group'} aria-label={field.description ?? field.title ?? key}>
          {#each options as option (option.const)}
            {@const checked = kind === 'single' ? values[key] === option.const : ((values[key] as string[] | undefined) ?? []).includes(option.const)}
            <label
              class="border-light-200 bg-light flex cursor-pointer items-start gap-2.5 rounded-xl border px-3 py-2 text-sm transition-colors {checked
                ? 'border-primary bg-primary/10'
                : 'hover:bg-light-100'}"
              onmouseenter={() => (previewing[key] = option.const)}
              onmouseleave={() => (previewing[key] = undefined)}
            >
              <input
                type={kind === 'single' ? 'radio' : 'checkbox'}
                name="{request.rpcId}-{key}"
                class="accent-primary mt-0.5 size-4 shrink-0"
                {checked}
                onclick={() => toggle(key, option.const, kind === 'multi')}
                onfocus={() => (previewing[key] = option.const)}
                onblur={() => (previewing[key] = undefined)}
              />
              <span class="flex min-w-0 flex-col">
                <span class="font-medium">{option.title ?? option.const}</span>
                {#if option.description}
                  <span class="text-muted text-xs">{option.description}</span>
                {/if}
              </span>
            </label>
          {/each}
        </div>
        {#if shown}
          <pre class="bg-light-100 border-light-200 max-h-48 overflow-auto rounded-lg border px-3 py-2 font-mono text-xs" data-testid="option-preview">{shown}</pre>
        {/if}
      {:else if kind === 'other'}
        <label class="flex items-center gap-2" title={field.description}>
          <span class="text-muted shrink-0 text-xs font-medium">{field.title ?? 'Other'}</span>
          <Input
            size="small"
            class="grow"
            value={(values[key] as string | undefined) ?? ''}
            oninput={(event) => (values[key] = event.currentTarget.value)}
            onkeydown={submitOnEnter}
            placeholder={field.description}
            aria-label={field.description ? `${field.title ?? 'Other'}: ${field.description}` : (field.title ?? 'Other')}
          />
        </label>
      {:else if kind === 'select'}
        <Select
          options={(field.enum ?? []).map((value, index) => ({ value, label: field.enumNames?.[index] ?? value }))}
          value={values[key] as string | undefined}
          onChange={(value) => (values[key] = value)}
          placeholder="Choose…"
        />
      {:else if kind === 'number'}
        <NumberInput
          value={values[key] as number | undefined}
          oninput={(event) => {
            const parsed = Number(event.currentTarget.value);
            values[key] = event.currentTarget.value === '' || Number.isNaN(parsed) ? undefined : parsed;
          }}
          min={field.minimum}
          max={field.maximum}
          aria-label={field.title ?? key}
        />
      {:else if kind === 'boolean'}
        <label class="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            class="accent-primary size-4"
            checked={values[key] === true}
            onchange={(event) => (values[key] = event.currentTarget.checked)}
          />
          <span>{field.title ?? key}{#if field.description}<span class="text-muted"> · {field.description}</span>{/if}</span>
        </label>
      {:else}
        <Input
          value={(values[key] as string | undefined) ?? ''}
          oninput={(event) => (values[key] = event.currentTarget.value)}
          onkeydown={submitOnEnter}
          aria-label={field.title ?? key}
        />
      {/if}
    </div>
  {/each}

  <div class="flex flex-wrap justify-end gap-2 pt-1">
    <Button size="small" shape="round" variant="outline" color="secondary" onclick={() => onRespond({ action: 'decline' })}>
      Skip
    </Button>
    <Button size="small" shape="round" disabled={missingRequired} onclick={answer}>
      Answer
    </Button>
  </div>
</section>
