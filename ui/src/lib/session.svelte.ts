import { AcpConnection, acpUrl, type ConnectionState, type RequestId } from './acp/connection.ts';
import { Transcript } from './acp/transcript.svelte.ts';
import type {
  ContentBlock,
  InitializeResponse,
  LoadSessionResponse,
  RequestPermissionRequest,
  SessionConfigOption,
  SessionNotification,
  SessionUpdate,
  StopReason,
} from './acp/types.ts';
import { api, ApiError, newerSession, type Proposal, type Session } from './api.ts';
import { sessions } from './sessions.svelte.ts';

export type PendingPermission = RequestPermissionRequest & { rpcId: RequestId };

const textPrompt = (text: string): ContentBlock[] => [{ type: 'text', text }];

/** One open session: its ACP connection, transcript, and the REST state that goes with it. */
export class SessionView {
  readonly id: string;

  session = $state<Session | null>(null);
  notFound = $state(false);
  transcript = $state(new Transcript());
  /** True once the first `session/load` has finished replaying. */
  loaded = $state(false);
  connection = $state<ConnectionState>('connecting');
  connectionError = $state<string | null>(null);

  configOptions = $state<SessionConfigOption[]>([]);
  /** The agent's model picker, if it offers one. */
  model = $derived(this.configOptions.find((option) => option.id === 'model' && option.type === 'select'));
  steeringSupported = $state(false);

  permissions = $state<PendingPermission[]>([]);
  proposals = $state<Proposal[]>([]);

  /** Prompts sent from here whose turn hasn't ended. Queued prompts all resolve when their shared turn ends. */
  promptsInFlight = $state(0);

  turnRunning = $derived(
    this.promptsInFlight > 0 || this.session?.phase === 'working' || this.session?.phase === 'waiting',
  );

  #conn: AcpConnection;
  /** Receives the history replay of a (re)load, and replaces `transcript` once the load completes. */
  #replay: Transcript | null = null;

  constructor(id: string) {
    this.id = id;
    this.#conn = new AcpConnection(acpUrl(), {
      onReady: (connection) => this.#onReady(connection),
      onNotification: (method, params) => this.#onNotification(method, params),
      onRequest: (rpcId, method, params) => this.#onRequest(rpcId, method, params),
      onStateChange: (state, error) => {
        this.connection = state;
        this.connectionError = error ?? null;
        if (state !== 'open') {
          // Requests from a dropped socket can no longer be answered; a reload re-sends live ones.
          this.permissions = [];
        }
      },
    });
  }

  start() {
    void this.refresh();
    this.#conn.connect();
    document.addEventListener('visibilitychange', this.#onVisible);
    window.addEventListener('online', this.#onVisible);
  }

  stop() {
    document.removeEventListener('visibilitychange', this.#onVisible);
    window.removeEventListener('online', this.#onVisible);
    this.#conn.close();
  }

  #onVisible = () => {
    if (document.visibilityState === 'visible') this.#conn.reconnectNow();
  };

  /** Refetches the REST side: the session itself and its proposals. */
  async refresh() {
    try {
      const [session, proposals] = await Promise.all([
        api.getSession(this.id),
        api.listProposals({ session: this.id }),
      ]);
      this.setSession(session);
      for (const proposal of proposals) this.upsertProposal(proposal);
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        this.notFound = true;
        this.stop();
      }
    }
  }

  async #onReady(connection: AcpConnection) {
    const init = await connection.request<InitializeResponse>('initialize', {
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
      clientInfo: { name: 'sloppod-ui', version: '0.0.0' },
    });
    this.steeringSupported = init?._meta?.steering?.supported === true;

    const rest = this.loaded ? this.refresh() : Promise.resolve();
    this.#replay = new Transcript();
    try {
      const response = await connection.request<LoadSessionResponse>('session/load', {
        sessionId: this.id,
        cwd: '/',
        mcpServers: [],
      });
      this.transcript = this.#replay;
      this.configOptions = response?.configOptions ?? [];
    } finally {
      this.#replay = null;
    }
    this.loaded = true;
    await rest;
  }

  #onNotification(method: string, params: unknown) {
    switch (method) {
      case 'session/update': {
        const notification = params as SessionNotification;
        if (notification.sessionId === this.id) this.#applyUpdate(notification.update);
        break;
      }
      case '_sloppod/session': {
        const { session } = params as { session: Session };
        if (session.id === this.id) this.setSession(session);
        break;
      }
      case '_sloppod/proposal': {
        const { proposal } = params as { proposal: Proposal };
        if (proposal.session_id === this.id) this.upsertProposal(proposal);
        break;
      }
      case '_sloppod/permission_resolved': {
        const { sessionId, toolCallId } = params as { sessionId: string; toolCallId: string };
        if (sessionId === this.id) {
          this.permissions = this.permissions.filter((request) => request.toolCall.toolCallId !== toolCallId);
        }
        break;
      }
    }
  }

  #onRequest(rpcId: RequestId, method: string, params: unknown) {
    if (method === 'session/request_permission') {
      const request = params as RequestPermissionRequest;
      if (request.sessionId === this.id) {
        this.permissions.push({ ...request, rpcId });
        return;
      }
    }
    this.#conn.respondError(rpcId, -32601, `Method not found: ${method}`);
  }

  #applyUpdate(update: SessionUpdate) {
    switch (update.sessionUpdate) {
      case 'config_option_update':
        this.configOptions = update.configOptions;
        return;
      case 'available_commands_update':
        return;
      case 'session_info_update':
        if (update.title && this.session) this.setSession({ ...this.session, title: update.title });
        return;
    }
    (this.#replay ?? this.transcript).apply(update);
  }

  setSession(session: Session) {
    this.session = newerSession(this.session, session);
    sessions.upsert(session);
  }

  upsertProposal(proposal: Proposal) {
    const index = this.proposals.findIndex((existing) => existing.id === proposal.id);
    if (index === -1) {
      this.proposals.push(proposal);
    } else {
      this.proposals[index] = proposal;
    }
  }

  /** Starts a turn. The returned promise settles with the stop reason when the turn ends. */
  async prompt(text: string): Promise<StopReason | undefined> {
    this.promptsInFlight++;
    try {
      const result = await this.#conn.request<{ stopReason: StopReason }>('session/prompt', {
        sessionId: this.id,
        prompt: textPrompt(text),
      });
      return result?.stopReason;
    } finally {
      this.promptsInFlight--;
    }
  }

  steer(text: string) {
    return this.#conn.request('_session/steering', { sessionId: this.id, prompt: textPrompt(text) });
  }

  cancel() {
    this.#conn.notify('session/cancel', { sessionId: this.id });
    // ACP: after cancelling, the client answers its open permission requests as cancelled.
    for (const request of this.permissions) {
      this.#conn.respond(request.rpcId, { outcome: { outcome: 'cancelled' } });
    }
    this.permissions = [];
  }

  async setConfigOption(configId: string, value: string) {
    const previous = this.configOptions;
    // Show the choice right away; the response carries the authoritative options.
    this.configOptions = previous.map((option) =>
      option.id === configId && option.type === 'select' ? { ...option, currentValue: value } : option,
    );
    try {
      const response = await this.#conn.request<{ configOptions?: SessionConfigOption[] }>('session/set_config_option', {
        sessionId: this.id,
        configId,
        value,
      });
      if (response?.configOptions) this.configOptions = response.configOptions;
    } catch (error) {
      this.configOptions = previous;
      throw error;
    }
  }

  answerPermission(request: PendingPermission, optionId: string) {
    this.#conn.respond(request.rpcId, { outcome: { outcome: 'selected', optionId } });
    this.permissions = this.permissions.filter((pending) => pending.rpcId !== request.rpcId);
  }
}
