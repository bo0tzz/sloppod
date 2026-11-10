<script lang="ts">
  import { Icon, Label } from '@immich/ui';
  import { mdiChevronDown, mdiMagnify } from '@mdi/js';
  import { Combobox } from 'bits-ui';
  import { untrack } from 'svelte';

  /** A repository the picker offers: the value is the `owner/name` a grant is made with, the label what it shows. */
  export type RepoOption = { value: string; label?: string };

  let {
    options,
    value = $bindable<string | undefined>(undefined),
    label,
    placeholder = 'Choose a repository',
    onChange,
  }: {
    options: (string | RepoOption)[];
    value?: string;
    label: string;
    placeholder?: string;
    onChange?: (value: string) => void;
  } = $props();

  const offered = $derived(
    options.map((option) =>
      typeof option === 'string' ? { value: option, label: option } : { ...option, label: option.label ?? option.value },
    ),
  );
  const items = $derived(offered.map(({ value: repo, label: text }) => ({ value: repo, label: text })));

  const id = $props.id();
  const inputId = `repo-picker-${id}`;
  let open = $state(false);
  /** What the field holds: the filter while the user types, the selection the rest of the time. */
  let text = $state('');

  // A value set from outside the picker (a token switch clears it) shows up in the field too. Reads of the field are
  // untracked: typing must not run this and reset what was typed.
  $effect(() => {
    const selected = value ?? '';
    untrack(() => {
      if (text !== selected) text = selected;
    });
  });

  const typed = $derived(text.trim());
  // The selection showing in the field is not a filter: only what the user typed narrows the list.
  const matches = $derived(
    typed === '' || typed === value ? offered : offered.filter((option) => option.label.toLowerCase().includes(typed.toLowerCase())),
  );

  function onOpenChange(next: boolean) {
    open = next;
    // A filter left over from looking at the list isn't what the row is set to.
    if (!next) text = value ?? '';
  }

  function chosen(next: string) {
    value = next === '' ? undefined : next;
    onChange?.(next);
  }
</script>

<div class="flex w-full flex-col gap-1">
  <Label {label} for={inputId} />
  <Combobox.Root
    type="single"
    bind:open
    {items}
    inputValue={text}
    value={value ?? ''}
    onValueChange={chosen}
    {onOpenChange}
  >
    <div
      class="flex w-full items-center rounded-lg bg-gray-100 ring-1 ring-gray-200 transition outline-none focus-within:ring-primary dark:bg-gray-800 dark:ring-neutral-700"
    >
      <span class="text-muted ps-3"><Icon icon={mdiMagnify} size="1.1em" aria-hidden /></span>
      <Combobox.Input
        id={inputId}
        class="w-full min-w-0 flex-1 bg-transparent py-2.5 ps-2 pe-2 text-sm outline-none"
        {placeholder}
        data-testid="repo-filter"
        onclick={() => (open = true)}
        oninput={(event) => (text = event.currentTarget.value)}
      />
      <Combobox.Trigger class="text-muted me-3 shrink-0 cursor-pointer" aria-label="Show repositories">
        <Icon icon={mdiChevronDown} size="1.1em" aria-hidden />
      </Combobox.Trigger>
    </div>
    <Combobox.Portal>
      <Combobox.Content
        class="text-dark dark:bg-primary-100 bg-light-100 z-55 max-h-80 w-(--bits-combobox-anchor-width) min-w-(--bits-combobox-anchor-width) overflow-y-auto rounded-lg border py-1 outline-none"
        sideOffset={4}
        data-testid="repo-options"
      >
        <Combobox.Viewport>
          {#each matches as option (option.value)}
            <Combobox.Item
              value={option.value}
              label={option.label}
              class="data-highlighted:bg-light-200 dark:data-highlighted:bg-primary-200 flex h-9 cursor-pointer items-center px-4 text-sm outline-none select-none"
            >
              {option.label}
            </Combobox.Item>
          {/each}
          {#if matches.length === 0}
            <div class="text-muted px-4 py-2 text-sm">No repository matches</div>
          {/if}
        </Combobox.Viewport>
      </Combobox.Content>
    </Combobox.Portal>
  </Combobox.Root>
</div>
