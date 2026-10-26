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
import type { Phase, ProposalStatus } from './api.ts';

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
