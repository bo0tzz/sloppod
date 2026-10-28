export type Phase = 'starting' | 'idle' | 'working' | 'waiting' | 'disconnected' | 'parked' | 'stopped';

export type Session = {
  id: string;
  title: string;
  repo: string | null;
  branch: string | null;
  grants: Grant[];
  phase: Phase;
  agent: string;
  /** 'work', or 'review' for a session that reviews a proposal. */
  kind: string;
  review: ReviewPolicy;
  created_at: string;
  updated_at: string;
  /** When something last happened in the session; the session list is ordered by it. */
  active_at: string;
  /** When it was deleted; it can be restored until it's purged a week later. */
  deleted_at?: string;
  /** What the pod is doing while the agent starts, e.g. cloning the repository. */
  agent_status?: string;
  /** Why the agent last failed to start or exited; it's started again meanwhile. */
  agent_error?: string;
  /** The session that spawned it, for a sub-session. */
  parent_id?: string;
  /** When the session, waiting for its usage limit to reset, carries on by itself. */
  limited_until?: string;
};

export type NewSession = {
  /** The first message. The runtime records it right away and derives the repository from it. */
  prompt: string;
  /** Absent, it's derived from the prompt; `null` means no repository. */
  repo?: string | null;
  title?: string;
  /** What it starts with, from the starting set's capabilities; all of them when absent. */
  capabilities?: string[];
  /** Instead of the user's default model. */
  model?: string;
};

/** What the runtime suggests a new session starts with for a first message as it's written (#232); advisory. */
export type StartingSet = {
  title: string;
  repo: string | null;
  capabilities: string[];
  /** A judgement made it, or matching the message's words did (the fallback). */
  source: 'judgement' | 'match';
};

export type Grant = {
  /** Set on a session's own grants, which the owner can change. */
  id?: string;
  capability: string;
  params: Record<string, unknown>;
  description?: string;
  created_at?: string;
  /** When it runs out; it stops counting then. */
  expires_at?: string;
};

/** A change to one of a session's grants: when it runs out (`null`: never), or narrowing it. */
export type GrantChange = { expires_at?: string | null; narrow?: true };

export type ProposalStatus =
  | 'in_review'
  | 'changes_requested'
  | 'pending'
  | 'approved'
  | 'rejected'
  | 'executed'
  | 'failed'
  | 'withdrawn';

export type PullRequestPayload = {
  repo: string;
  base: string;
  head: string;
  title: string;
  body: string;
  draft: boolean;
  /** Merge once required checks pass; the agent asks, the reviewer decides. */
  auto_merge?: boolean;
};
export type RepoGrantPayload = { repo: string; reason: string; branch: string };
export type IssueDraft = { title: string; body: string; labels: string[] };
export type IssuesPayload = { repo: string; issues: IssueDraft[] };
export type IssueUpdate = {
  number: number;
  comment?: string;
  close?: 'completed' | 'not_planned' | 'duplicate';
  reopen?: boolean;
  title?: string;
  body?: string;
  add_labels?: string[];
  remove_labels?: string[];
};
export type IssueUpdatesPayload = { repo: string; updates: IssueUpdate[] };
export type ProposalEdits = {
  title?: string;
  body?: string;
  auto_merge?: boolean;
  issues?: IssueDraft[];
  updates?: IssueUpdate[];
  /** `sloppod.repo.act_as_owner`: which of the user's GitHub tokens to act with. */
  credential?: string;
};

export type Proposal = {
  id: string;
  session_id: string;
  /** `github.pr.create`, `github.issue.create` and `sloppod.repo.grant` so far; others render generically. */
  kind: string;
  status: ProposalStatus;
  payload: Record<string, unknown>;
  result?: {
    url?: string;
    number?: number;
    issues?: { title: string; url: string; number: number }[];
    pull_requests?: { url: string; number: number }[];
    message?: string;
    failed?: { title: string; error: string }[];
    /** `sloppod.repo.act_as_owner`: whose token it ended up acting with. */
    login?: string;
  };
  error?: string;
  reason?: string;
  created_at: string;
  decided_at?: string;
  /** The user who decided, or `standing-approval:<id>`. */
  decided_by?: string;
  /** The session reviewing it, or that did. */
  review_session_id?: string;
};

/** A pre-approved proposal pattern: matching proposals are approved as they're made. */
export type StandingApproval = {
  id: string;
  kind: string;
  /** A repository as owner/name; absent matches any. */
  target?: string;
  session_id?: string;
  edits: { auto_merge?: boolean };
  expires_at?: string;
  created_at: string;
};

export type NewStandingApproval = Omit<StandingApproval, 'id' | 'created_at'>;

/** A reviewer's finding; an id carried over from an earlier round marks the same finding. */
export type ReviewFinding = {
  id: string;
  severity: 'p0' | 'p1' | 'p2';
  category: string;
  title: string;
  explanation: string;
  path?: string;
  line?: number;
  end_line?: number;
  suggested_fix?: string;
};

/** One round of a proposal's review by another model (ADR 24). */
export type Review = {
  round: number;
  head_sha: string;
  /** The author's replies to the previous round's findings. */
  replies?: string;
  /** Absent while the round runs. */
  verdict?: 'pass' | 'changes';
  summary?: string;
  findings: ReviewFinding[];
  /** Ids of the previous round's findings that no longer hold. */
  resolved: string[];
  created_at: string;
  finished_at?: string;
};

/** Pull requests add their diff and commits, issue updates the current text of the issues they rewrite. */
export type ProposalDetail = Proposal & {
  diff?: string;
  commits?: { sha: string; message: string }[];
  /** Stacks: each pull request's diff and commits, bottom first. */
  layers?: { diff: string; commits: { sha: string; message: string }[] }[];
  /** Issue updates: each issue whose title or body they replace, as it is now. */
  current?: { number: number; title: string; body: string }[];
};

/** Pull requests that build on each other, bottom first; each merges into the one below it. */
export type StackPayload = {
  repo: string;
  base: string;
  pull_requests: { head: string; base: string; head_sha: string; title: string; body: string; draft: boolean }[];
  auto_merge?: boolean;
};

export type CredentialKind = 'claude' | 'github-app' | 'memini' | 'chatgpt' | 'github-user';

export type Credential = {
  id: string;
  kind: CredentialKind;
  name: string;
  /** Strings, except a `github-user`'s, which is a {@link GitHubUserConfig}. */
  config: Record<string, unknown>;
  default: boolean;
  created_at: string;
};

/** What the runtime learned of a GitHub token when it was added. */
export type GitHubUserConfig = {
  login: string;
  expires_at: string | null;
  repositories: string[];
  /** The list stops short: the token reaches more. */
  more_repositories: boolean;
};

export type NewCredential = {
  kind: CredentialKind;
  name: string;
  config: Record<string, string>;
  secret: string;
};

/** What's left of an inference credential's usage limits, as last reported. `utilization` is a fraction, usually 0 to 1. */
export type Quota = {
  credential_id: string;
  kind: CredentialKind;
  name: string;
  /** The provider refuses its requests now, until `resets_at`. */
  limited: boolean;
  resets_at: string | null;
  windows: QuotaWindow[];
  observed_at: string;
};

export type QuotaStatus = 'allowed' | 'warning' | 'rejected';

export type QuotaWindow = {
  /** `five_hour` and `seven_day` for the session and weekly limits. */
  window: string;
  utilization: number | null;
  resets_at: string | null;
  status: QuotaStatus;
};

/** Defaults for new sessions. `models` is what the agent offered most recently; empty before the first session. */
export type Settings = {
  model: string | null;
  models: { value: string; name: string }[];
  review: ReviewPolicy;
  /** The user's timezone, e.g. `Europe/Amsterdam`, for the times agents are told and their pods' clocks. */
  timezone: string | null;
};

/** Whether proposals are reviewed by another model before a human sees them. */
export type ReviewPolicy = 'off' | 'required';

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const response = await fetch(`/api/v1${path}`, {
    method,
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!response.ok) {
    let message = `${response.status} ${response.statusText}`;
    try {
      const error = await response.json();
      if (typeof error?.error === 'string') {
        message = error.error;
      }
    } catch {
      // not JSON; keep the status line
    }
    throw new ApiError(response.status, message);
  }
  if (response.status === 204) {
    return undefined as T;
  }
  return response.json();
}

export const api = {
  listSessions: () => call<Session[]>('GET', '/sessions'),
  getSession: (id: string) => call<Session>('GET', `/sessions/${encodeURIComponent(id)}`),
  createSession: (body: NewSession) => call<Session>('POST', '/sessions', body),
  suggestSession: (prompt: string) => call<StartingSet>('POST', '/sessions/suggest', { prompt }),
  deleteSession: (id: string) => call<void>('DELETE', `/sessions/${encodeURIComponent(id)}`),
  listDeletedSessions: () => call<Session[]>('GET', '/sessions/deleted'),
  restoreSession: (id: string) => call<Session>('POST', `/sessions/${encodeURIComponent(id)}/restore`, {}),
  parkSession: (id: string) => call<Session>('POST', `/sessions/${encodeURIComponent(id)}/park`, {}),
  wakeSession: (id: string) => call<Session>('POST', `/sessions/${encodeURIComponent(id)}/wake`, {}),
  /** Write access to `repo`; with `credential`, one of the user's GitHub tokens, the session acts on it as them (ADR 29). */
  addSessionRepo: (id: string, repo: string, credential?: string) =>
    call<Session>('POST', `/sessions/${encodeURIComponent(id)}/repos`, { repo, credential }),
  grantSession: (id: string, capability: string, expires_at?: string) =>
    call<Session>('POST', `/sessions/${encodeURIComponent(id)}/grants`, { capability, expires_at }),
  changeGrant: (id: string, grant: string, change: GrantChange) =>
    call<Session>('PATCH', `/sessions/${encodeURIComponent(id)}/grants/${encodeURIComponent(grant)}`, change),
  revokeGrant: (id: string, grant: string) =>
    call<Session>('DELETE', `/sessions/${encodeURIComponent(id)}/grants/${encodeURIComponent(grant)}`),
  listRepos: () => call<string[]>('GET', '/repos'),
  /** Who we are, and the runtime's version and the commit or tag it was built from, if it knows them. */
  me: () => call<{ user: string; version: string | null; build: string | null }>('GET', '/me'),
  getSettings: () => call<Settings>('GET', '/settings'),
  updateSettings: (body: { model?: string | null; review?: ReviewPolicy; timezone?: string }) =>
    call<Settings>('PUT', '/settings', body),
  setSessionReview: (id: string, review: ReviewPolicy) =>
    call<Session>('PUT', `/sessions/${encodeURIComponent(id)}/review`, { review }),
  listCredentials: () => call<Credential[]>('GET', '/credentials'),
  listQuotas: () => call<Quota[]>('GET', '/quotas'),
  listStandingApprovals: () => call<StandingApproval[]>('GET', '/standing-approvals'),
  createStandingApproval: (approval: NewStandingApproval) =>
    call<StandingApproval>('POST', '/standing-approvals', approval),
  revokeStandingApproval: (id: string) => call<void>('DELETE', `/standing-approvals/${encodeURIComponent(id)}`),
  createCredential: (body: NewCredential) => call<Credential>('POST', '/credentials', body),
  makeDefaultCredential: (id: string) => call<Credential>('POST', `/credentials/${encodeURIComponent(id)}/default`, {}),
  deleteCredential: (id: string) => call<void>('DELETE', `/credentials/${encodeURIComponent(id)}`),
  listProposals: (query: { session?: string; status?: ProposalStatus }) => {
    const params = new URLSearchParams();
    if (query.session) params.set('session', query.session);
    if (query.status) params.set('status', query.status);
    return call<Proposal[]>('GET', `/proposals?${params}`);
  },
  getProposal: (id: string) => call<ProposalDetail>('GET', `/proposals/${encodeURIComponent(id)}`),
  listReviews: (id: string) => call<Review[]>('GET', `/proposals/${encodeURIComponent(id)}/reviews`),
  approveProposal: (id: string, edits: ProposalEdits = {}) =>
    call<Proposal>('POST', `/proposals/${encodeURIComponent(id)}/approve`, edits),
  rejectProposal: (id: string, reason?: string) =>
    call<Proposal>('POST', `/proposals/${encodeURIComponent(id)}/reject`, reason ? { reason } : {}),
};

/** Keeps whichever copy of a session is newer, so a slow REST response can't undo a live update. */
export function newerSession(current: Session | null | undefined, incoming: Session): Session {
  if (current && Date.parse(current.updated_at) > Date.parse(incoming.updated_at)) {
    return current;
  }
  return incoming;
}

/** How far along a proposal is; it only ever moves forward, apart from going back and forth between review rounds. */
const proposalStage: Record<ProposalStatus, number> = {
  in_review: 0,
  changes_requested: 0,
  pending: 1,
  approved: 2,
  rejected: 3,
  executed: 3,
  failed: 3,
  withdrawn: 3,
};

/** Keeps whichever copy of a proposal is further along, so a stale copy (e.g. a slow refetch) can't undo a decision. */
export function newerProposal(current: Proposal | undefined, incoming: Proposal): Proposal {
  if (current && proposalStage[current.status] > proposalStage[incoming.status]) {
    return current;
  }
  return incoming;
}

export function sortSessions(sessions: Session[]): Session[] {
  return [...sessions].sort((a, b) => Date.parse(b.active_at) - Date.parse(a.active_at));
}
