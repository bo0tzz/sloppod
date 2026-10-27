import {
  mdiAlertCircleOutline,
  mdiHelpCircleOutline,
  mdiSourcePull,
  mdiSourceRepository,
  mdiCheckCircle,
  mdiCircleOutline,
  mdiClockOutline,
  mdiCloseCircle,
  mdiConsole,
  mdiDeleteOutline,
  mdiFileDocumentOutline,
  mdiFileMoveOutline,
  mdiLightbulbOutline,
  mdiMagnify,
  mdiPencilOutline,
  mdiProgressClock,
  mdiSwapHorizontal,
  mdiToolboxOutline,
  mdiWeb,
} from '@mdi/js';
import type { Color } from '@immich/ui';
import type { ConfigSelectGroup, ConfigSelectOption, PlanEntry, ToolCallStatus, ToolKind } from './acp/types.ts';
import type { CredentialKind, Grant, Phase, Proposal, ProposalStatus } from './api.ts';

export const phases: Record<Phase, { label: string; color: Color; description: string }> = {
  starting: { label: 'Starting', color: 'info', description: 'The pod is starting or the agent has not connected yet' },
  idle: { label: 'Idle', color: 'secondary', description: 'Waiting for a prompt' },
  working: { label: 'Working', color: 'primary', description: 'A turn is running' },
  waiting: { label: 'Waiting', color: 'warning', description: 'Blocked on a permission request' },
  disconnected: { label: 'Disconnected', color: 'danger', description: 'The pod went away; the runtime is bringing it back' },
  parked: { label: 'Parked', color: 'secondary', description: 'No pod; sending a message or Wake brings it back' },
  stopped: { label: 'Stopped', color: 'secondary', description: 'The session has stopped' },
};

export const toolKindIcons: Record<ToolKind, string> = {
  read: mdiFileDocumentOutline,
  edit: mdiPencilOutline,
  delete: mdiDeleteOutline,
  move: mdiFileMoveOutline,
  search: mdiMagnify,
  execute: mdiConsole,
  think: mdiLightbulbOutline,
  fetch: mdiWeb,
  switch_mode: mdiSwapHorizontal,
  other: mdiToolboxOutline,
};

export const toolStatuses: Record<ToolCallStatus, { icon: string; label: string; class: string }> = {
  pending: { icon: mdiClockOutline, label: 'Pending', class: 'text-muted' },
  in_progress: { icon: mdiProgressClock, label: 'Running', class: 'text-primary animate-pulse' },
  completed: { icon: mdiCheckCircle, label: 'Completed', class: 'text-success' },
  failed: { icon: mdiCloseCircle, label: 'Failed', class: 'text-danger' },
};

export const planStatuses: Record<PlanEntry['status'], { icon: string; class: string }> = {
  pending: { icon: mdiCircleOutline, class: 'text-muted' },
  in_progress: { icon: mdiProgressClock, class: 'text-primary' },
  completed: { icon: mdiCheckCircle, class: 'text-success' },
};

export const proposalStatuses: Record<ProposalStatus, { label: string; color: Color }> = {
  in_review: { label: 'In review', color: 'info' },
  changes_requested: { label: 'Changes requested', color: 'secondary' },
  pending: { label: 'Pending review', color: 'warning' },
  approved: { label: 'Approved', color: 'info' },
  rejected: { label: 'Rejected', color: 'secondary' },
  executed: { label: 'Done', color: 'success' },
  failed: { label: 'Failed', color: 'danger' },
  withdrawn: { label: 'Withdrawn', color: 'secondary' },
};

const executedLabels: Record<string, string> = {
  'github.pr.create': 'Opened',
  'github.stack.create': 'Opened',
  'github.issue.create': 'Filed',
  'sloppod.repo.grant': 'Granted',
};

export function proposalStatus(proposal: Proposal): { label: string; color: Color } {
  const status = proposalStatuses[proposal.status] ?? { label: proposal.status, color: 'secondary' };
  return proposal.status === 'executed' ? { ...status, label: executedLabels[proposal.kind] ?? status.label } : status;
}

const payloadText = (value: unknown) => (typeof value === 'string' ? value : '');

/** How a proposal reads in cards and the modal, by kind. Unknown kinds fall back to their kind name. */
export function proposalSummary(proposal: Proposal): { label: string; title: string; detail?: string; icon: string } {
  const payload = proposal.payload;
  switch (proposal.kind) {
    case 'github.pr.create':
      return {
        label: 'Pull request',
        title: payloadText(payload.title),
        detail: `${payloadText(payload.repo)}: ${payloadText(payload.head)} → ${payloadText(payload.base)}`,
        icon: mdiSourcePull,
      };
    case 'github.stack.create': {
      const layers = Array.isArray(payload.pull_requests) ? (payload.pull_requests as { title?: unknown }[]) : [];
      return {
        label: 'Stacked pull requests',
        title: `${layers.length} pull requests, from “${payloadText(layers[0]?.title)}”`,
        detail: `${payloadText(payload.repo)} → ${payloadText(payload.base)}`,
        icon: mdiSourcePull,
      };
    }
    case 'github.issue.create': {
      const issues = Array.isArray(payload.issues) ? (payload.issues as { title?: unknown }[]) : [];
      return {
        label: 'Issues',
        title: issues.length === 1 ? payloadText(issues[0].title) : `${issues.length} issues`,
        detail: payloadText(payload.repo),
        icon: mdiAlertCircleOutline,
      };
    }
    case 'sloppod.repo.grant':
      return {
        label: 'Repository access',
        title: `Write access to ${payloadText(payload.repo)}`,
        detail: payloadText(payload.reason) || undefined,
        icon: mdiSourceRepository,
      };
    default:
      return { label: 'Proposal', title: proposal.kind, icon: mdiHelpCircleOutline };
  }
}

/** MCP tools arrive titled `mcp__<server>__<tool>`; show the tool name and say which server it's from. */
export function toolTitle(title: string): { title: string; server?: string } {
  const match = /^mcp__(.+?)__(.+)$/.exec(title);
  return match ? { title: match[2].replaceAll('_', ' '), server: match[1] } : { title };
}

/** Tool calls often start with `rawInput: {}` before the input streams in; that isn't worth showing. */
export function hasInput(rawInput: unknown): boolean {
  if (rawInput === undefined || rawInput === null) return false;
  return typeof rawInput !== 'object' || Object.keys(rawInput).length > 0;
}

export type CredentialKindSpec = {
  kind: CredentialKind;
  title: string;
  noun: string;
  description: string;
  empty: string;
  fields: { key: string; label: string; type?: string; placeholder?: string; description?: string; optional?: boolean }[];
  secret: { label: string; description?: string; multiline?: boolean; placeholder?: string };
};

export const credentialKinds: CredentialKindSpec[] = [
  {
    kind: 'claude',
    title: 'Claude accounts',
    noun: 'Claude account',
    description: 'Model access for your sessions.',
    empty: "No Claude account yet. Sessions can't start without one.",
    fields: [],
    secret: { label: 'Token', description: 'Run `claude setup-token` and paste the token it prints.' },
  },
  {
    kind: 'github-app',
    title: 'GitHub Apps',
    noun: 'GitHub App',
    description: 'Repository access: sessions can work on the repositories the app is installed on.',
    empty: "No GitHub App yet. Sessions can't work on repositories without one.",
    fields: [{ key: 'app_id', label: 'App ID', placeholder: '123456' }],
    secret: {
      label: 'Private key',
      description: 'The PEM file GitHub generated for the app.',
      multiline: true,
      placeholder: 'Paste the .pem file here',
    },
  },
  {
    kind: 'memini',
    title: 'memini',
    noun: 'memini server',
    description: 'Shared memory across sessions.',
    empty: 'No memini server yet.',
    fields: [
      { key: 'url', label: 'URL', type: 'url', placeholder: 'https://memini.example.com' },
      {
        key: 'home',
        label: 'Your namespace',
        placeholder: 'personal/you',
        description: 'Your personal memini namespace; sessions read it alongside their project.',
        optional: true,
      },
    ],
    secret: { label: 'Admin key' },
  },
  {
    kind: 'chatgpt',
    title: 'ChatGPT logins',
    noun: 'ChatGPT login',
    description: 'Codex, for reviewing proposals before they reach you.',
    empty: 'No ChatGPT login yet.',
    fields: [],
    secret: {
      label: 'auth.json',
      description:
        "Run `codex login` with a fresh `CODEX_HOME` (e.g. `CODEX_HOME=$(mktemp -d) codex login`) and paste that directory's auth.json. Use a login only sloppod has: it refreshes the tokens, and another copy would invalidate them.",
      multiline: true,
      placeholder: 'Paste auth.json here',
    },
  },
];

/** What a grant lets a session do, in plain words. */
export function grantLabel(grant: Grant): string {
  const param = (name: string) => (typeof grant.params[name] === 'string' ? (grant.params[name] as string) : '');
  switch (grant.capability) {
    case 'model':
      return 'the model API';
    case 'codex':
      return 'Codex';
    case 'github':
      return 'read GitHub';
    case 'packages':
      return 'package registries';
    case 'repo':
      return param('access') === 'rw'
        ? `push to ${param('branch')} on ${param('repo')}`
        : `read ${param('repo')}`;
    case 'memory':
      return `memory (${param('namespace')})`;
    default:
      return grant.capability;
  }
}

/** An option's name. The agent describes its `default` model with the model that currently is, so say which. */
function configOptionName(option: ConfigSelectOption): string {
  if (option.value !== 'default' || !option.description) return option.name;
  return `${option.name.replace(/\s*\(.*\)$/, '')} · ${option.description}`;
}

/** Flattens a config select's options for a plain select; grouped options get their group's name. */
export function configSelectOptions(options: ConfigSelectOption[] | ConfigSelectGroup[]): { value: string; label: string }[] {
  return options.flatMap((entry) =>
    'group' in entry
      ? entry.options.map((option) => ({ value: option.value, label: `${entry.name}: ${configOptionName(option)}` }))
      : [{ value: entry.value, label: configOptionName(entry) }],
  );
}
