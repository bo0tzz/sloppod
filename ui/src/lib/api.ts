export type Phase = 'starting' | 'idle' | 'working' | 'waiting' | 'disconnected' | 'stopped';

export type Session = {
  id: string;
  title: string;
  repo: string | null;
  branch: string | null;
  grants: Grant[];
  phase: Phase;
  created_at: string;
  updated_at: string;
  /** When something last happened in the session; the session list is ordered by it. */
  active_at: string;
};

export type NewSession = {
  /** The first message. The runtime records it right away and derives the repository from it. */
  prompt: string;
  repo?: string;
  title?: string;
};

export type Grant = { capability: string; params: Record<string, unknown>; description?: string };

export type ProposalStatus = 'pending' | 'approved' | 'rejected' | 'executed' | 'failed' | 'withdrawn';

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
export type ProposalEdits = { title?: string; body?: string; auto_merge?: boolean; issues?: IssueDraft[] };

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
  };
  error?: string;
  reason?: string;
  created_at: string;
  decided_at?: string;
  /** The user who decided, or `standing-approval:<id>`. */
  decided_by?: string;
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

/** Pull requests add their diff and commits; other kinds add nothing. */
export type ProposalDetail = Proposal & {
  diff?: string;
  commits?: { sha: string; message: string }[];
  /** Stacks: each pull request's diff and commits, bottom first. */
  layers?: { diff: string; commits: { sha: string; message: string }[] }[];
};

/** Pull requests that build on each other, bottom first; each merges into the one below it. */
export type StackPayload = {
  repo: string;
  base: string;
  pull_requests: { head: string; base: string; head_sha: string; title: string; body: string; draft: boolean }[];
};

export type CredentialKind = 'claude' | 'github-app' | 'memini' | 'chatgpt';

export type Credential = {
  id: string;
  kind: CredentialKind;
  name: string;
  config: Record<string, string>;
  default: boolean;
  created_at: string;
};

export type NewCredential = {
  kind: CredentialKind;
  name: string;
  config: Record<string, string>;
  secret: string;
};

/** Defaults for new sessions. `models` is what the agent offered most recently; empty before the first session. */
export type Settings = {
  model: string | null;
  models: { value: string; name: string }[];
};

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
  deleteSession: (id: string) => call<void>('DELETE', `/sessions/${encodeURIComponent(id)}`),
  addSessionRepo: (id: string, repo: string) =>
    call<Session>('POST', `/sessions/${encodeURIComponent(id)}/repos`, { repo }),
  listRepos: () => call<string[]>('GET', '/repos'),
  me: () => call<{ user: string }>('GET', '/me'),
  getSettings: () => call<Settings>('GET', '/settings'),
  updateSettings: (body: { model: string | null }) => call<Settings>('PUT', '/settings', body),
  listCredentials: () => call<Credential[]>('GET', '/credentials'),
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

/** How far along a proposal is; it only ever moves forward. */
const proposalStage: Record<ProposalStatus, number> = { pending: 0, approved: 1, rejected: 2, executed: 2, failed: 2, withdrawn: 2 };

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
