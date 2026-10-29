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
import { timeAgo } from './format.ts';

export const phases: Record<Phase, { label: string; color: Color; description: string }> = {
  starting: { label: 'Starting', color: 'info', description: 'The pod is starting or the agent has not connected yet' },
  idle: { label: 'Idle', color: 'secondary', description: 'Waiting for a prompt' },
  working: { label: 'Working', color: 'primary', description: 'A turn is running' },
  waiting: { label: 'Waiting', color: 'warning', description: 'Blocked on a permission request or a question' },
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

/**
 * What each proposal kind is called: `title` heads its modal, `plural` names its standing approvals, and `executed`
 * is its status once carried out. Kinds missing here show their kind name.
 */
export const proposalKinds: Record<string, { title: string; plural: string; executed: string }> = {
  'github.pr.create': { title: 'Pull request proposal', plural: 'Pull requests', executed: 'Opened' },
  'github.stack.create': { title: 'Stacked pull requests', plural: 'Stacked pull requests', executed: 'Opened' },
  'github.issue.create': { title: 'Issues proposal', plural: 'Issues', executed: 'Filed' },
  'github.issue.update': { title: 'Issue updates', plural: 'Issue updates', executed: 'Updated' },
  'github.pr.update': { title: 'Pull request updates', plural: 'Pull request updates', executed: 'Updated' },
  'sloppod.repo.grant': { title: 'Repository access', plural: 'Repository access', executed: 'Granted' },
  'sloppod.repo.act_as_owner': { title: 'Acting as you', plural: 'Acting as you', executed: 'Granted' },
  'sloppod.session.report': { title: 'Report to the parent session', plural: 'Reports', executed: 'Delivered' },
  'sloppod.code_review': { title: 'Code review', plural: 'Code reviews', executed: 'Reviewed' },
};

export function proposalStatus(proposal: Proposal): { label: string; color: Color } {
  const status = proposalStatuses[proposal.status] ?? { label: proposal.status, color: 'secondary' };
  const executed = proposalKinds[proposal.kind]?.executed;
  return proposal.status === 'executed' && executed ? { ...status, label: executed } : status;
}

const payloadText = (value: unknown) => (typeof value === 'string' ? value : '');

/** Whose GitHub token a proposal acts with (ADR 29), by login; none when it acts as the GitHub App. */
export function actingAs(proposal: Proposal): string | undefined {
  return typeof proposal.payload.as === 'string' ? proposal.payload.as : undefined;
}

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
    case 'github.issue.update': {
      const updates = Array.isArray(payload.updates) ? (payload.updates as { number?: unknown }[]) : [];
      return {
        label: 'Issue updates',
        title: updates.length === 1 ? `#${String(updates[0].number)}` : `${updates.length} issues`,
        detail: payloadText(payload.repo),
        icon: mdiAlertCircleOutline,
      };
    }
    case 'github.pr.update': {
      const updates = Array.isArray(payload.updates) ? (payload.updates as { number?: unknown }[]) : [];
      return {
        label: 'Pull request updates',
        title: updates.length === 1 ? `#${String(updates[0].number)}` : `${updates.length} pull requests`,
        detail: payloadText(payload.repo),
        icon: mdiSourcePull,
      };
    }
    case 'sloppod.repo.act_as_owner':
      return {
        label: 'Acting as you',
        // The token picked when approving, once it's carried out.
        title: `Act as ${payloadText(proposal.result?.login ?? payload.login) || 'you'} on ${payloadText(payload.repo)}`,
        detail: payloadText(payload.reason) || undefined,
        icon: mdiSourceRepository,
      };
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
  /** What deleting one does, when it isn't only that new sessions can't use it. */
  deletion?: string;
};

export const credentialKinds: CredentialKindSpec[] = [
  {
    kind: 'claude',
    title: 'Claude accounts',
    noun: 'Claude account',
    description:
      "Model access for your sessions. A new session's model grant lists them all, the default first, and uses the first; change a session's in its grants list. The runtime's own judgements use the default.",
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
    kind: 'github-user',
    title: 'GitHub accounts',
    noun: 'GitHub account',
    description:
      "Your own GitHub identity, for sessions you let act as you where the GitHub App can't. Nothing uses it yet.",
    empty: 'No GitHub account yet.',
    fields: [],
    secret: {
      label: 'Fine-grained personal access token',
      description:
        'Make one under GitHub → Settings → Developer settings → Fine-grained tokens, limited to the repositories sessions may work on as you. Classic tokens are refused: they reach everything you can.',
    },
  },
  {
    kind: 'token',
    title: 'Capability tokens',
    noun: 'token',
    description: "Secrets your own capabilities' servers take. A capability below can send one with every request.",
    empty: 'No token yet.',
    fields: [
      {
        key: 'header',
        label: 'Header',
        placeholder: 'X-API-Key',
        description: 'Sent as this header. Left out, the token is sent as Authorization: Bearer.',
        optional: true,
      },
    ],
    secret: { label: 'Token' },
    deletion:
      'Capabilities that use it stop sending it at once, also to sessions that already have them, and their servers may refuse those requests. Its secret is deleted with it.',
  },
  {
    kind: 'chatgpt',
    title: 'ChatGPT logins',
    noun: 'ChatGPT login',
    description:
      "Codex, for reviewing proposals before they reach you. A new codex grant lists them all, the default first, and uses the first.",
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

/** How a token's expiry reads, and whether it needs attention: within a week, or past. */
export function tokenExpiry(expiresAt: string | null, now = Date.now()): { text: string; level: 'ok' | 'soon' | 'expired' } {
  if (!expiresAt) return { text: "Doesn't expire", level: 'ok' };
  const left = Date.parse(expiresAt) - now;
  if (left <= 0) return { text: `Expired ${timeAgo(expiresAt, now)}`, level: 'expired' };
  return { text: `Expires ${timeAgo(expiresAt, now)}`, level: left < 7 * 24 * 3600_000 ? 'soon' : 'ok' };
}
