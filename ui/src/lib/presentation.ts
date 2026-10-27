import type { Grant } from './api.ts';
import {
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
import type { PlanEntry, ToolCallStatus, ToolKind } from './acp/types.ts';
import type { CredentialKind, Phase, ProposalStatus } from './api.ts';

export const phases: Record<Phase, { label: string; color: Color; description: string }> = {
  starting: { label: 'Starting', color: 'info', description: 'The pod is starting or the agent has not connected yet' },
  idle: { label: 'Idle', color: 'secondary', description: 'Waiting for a prompt' },
  working: { label: 'Working', color: 'primary', description: 'A turn is running' },
  waiting: { label: 'Waiting', color: 'warning', description: 'Blocked on a permission request' },
  disconnected: { label: 'Disconnected', color: 'danger', description: 'The pod went away; the runtime is bringing it back' },
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
  pending: { label: 'Pending review', color: 'warning' },
  approved: { label: 'Approved', color: 'info' },
  rejected: { label: 'Rejected', color: 'secondary' },
  executed: { label: 'Opened', color: 'success' },
  failed: { label: 'Failed', color: 'danger' },
};

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
];

/** What a grant lets a session do, in plain words. */
export function grantLabel(grant: Grant): string {
  const param = (name: string) => (typeof grant.params[name] === 'string' ? (grant.params[name] as string) : '');
  switch (grant.capability) {
    case 'model':
      return 'the model API';
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
