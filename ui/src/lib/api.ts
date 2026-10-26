export type Phase = 'starting' | 'idle' | 'working' | 'waiting' | 'disconnected' | 'stopped';

export type Session = {
  id: string;
  title: string;
  repo: string;
  branch: string;
  phase: Phase;
  created_at: string;
  updated_at: string;
};

export type NewSession = {
  repo: string;
  title?: string;
  prompt?: string;
};

export type ProposalStatus = 'pending' | 'approved' | 'rejected' | 'executed' | 'failed';

export type Proposal = {
  id: string;
  session_id: string;
  kind: 'github.pr.create';
  status: ProposalStatus;
  payload: { repo: string; base: string; head: string; title: string; body: string; draft: boolean };
  result?: { url: string; number: number };
  error?: string;
  reason?: string;
  created_at: string;
  decided_at?: string;
};

export type ProposalDetail = Proposal & {
  diff: string;
  commits: { sha: string; message: string }[];
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
  listRepos: () => call<string[]>('GET', '/repos'),
  listProposals: (query: { session?: string; status?: ProposalStatus }) => {
    const params = new URLSearchParams();
    if (query.session) params.set('session', query.session);
    if (query.status) params.set('status', query.status);
    return call<Proposal[]>('GET', `/proposals?${params}`);
  },
  getProposal: (id: string) => call<ProposalDetail>('GET', `/proposals/${encodeURIComponent(id)}`),
  approveProposal: (id: string, edits: { title?: string; body?: string } = {}) =>
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

export function sortSessions(sessions: Session[]): Session[] {
  return [...sessions].sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at));
}
