import {
  AcpConnection,
  acpUrl,
  ConnectionClosedError,
  type ConnectionState,
  type RequestId,
} from './acp/connection.ts';
import { Transcript } from './acp/transcript.svelte.ts';
import type {
  ContentBlock,
  InitializeResponse,
  LoadSessionResponse,
  ElicitationRequest,
  ElicitationResponse,
  RequestPermissionRequest,
  SessionConfigOption,
  SessionNotification,
  SessionUpdate,
  StopReason,
} from './acp/types.ts';
import { api, ApiError, newerProposal, newerSession, type Proposal, type Session } from './api.ts';
import { sessions } from './sessions.svelte.ts';

export type PendingPermission = RequestPermissionRequest & { rpcId: RequestId };
export type PendingQuestion = ElicitationRequest & { rpcId: RequestId };
/** A message waiting for the running turn to end (`_sloppod/queue`). */
export type QueuedMessage = { id: string; prompt: ContentBlock[] };

export const queuedText = (message: QueuedMessage) =>
  message.prompt.map((block) => (block.type === 'text' ? block.text : '')).join('');

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
  /** Why the session's REST side (the session and its proposals) didn't load; it's retried meanwhile. */
  loadError = $state<string | null>(null);
  #loadFailures = 0;
  #loadRetry: ReturnType<typeof setTimeout> | undefined;
  #stopped = false;

  configOptions = $state<SessionConfigOption[]>([]);
  /** The agent's model picker, if it offers one. */
  model = $derived(this.configOptions.find((option) => option.id === 'model' && option.type === 'select'));
  steeringSupported = $state(false);

  permissions = $state<PendingPermission[]>([]);
  /** Open questions from the agent (form elicitations), answered like permission requests. */
  questions = $state<PendingQuestion[]>([]);
  proposals = $state<Proposal[]>([]);

  /** Prompts sent from here whose turn hasn't ended. Queued prompts all resolve when their shared turn ends. */
  promptsInFlight = $state(0);
  /** Messages waiting for the running turn to end. */
  queue = $state<QueuedMessage[]>([]);

  turnRunning = $derived(
    this.promptsInFlight > 0 || this.session?.phase === 'working' || this.session?.phase === 'waiting',
  );

  #conn: AcpConnection;
  /** Receives the history replay of a (re)load, and replaces `transcript` once the load completes. */
  #replay: Transcript | null = null;
  /** Messages a dropped connection cut off before the runtime confirmed them; the next load settles each. */
  #unconfirmed: { id: string; text: string }[] = [];
  /** Messages that couldn't be delivered, kept until the user puts each back or dismisses it. */
  undelivered = $state<{ id: string; text: string }[]>([]);

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
          this.questions = [];
        }
      },
    });
  }

  start() {
    this.#stopped = false;
    void this.refresh();
    this.#conn.connect();
    document.addEventListener('visibilitychange', this.#onVisible);
    window.addEventListener('online', this.#onVisible);
  }

  stop() {
    this.#stopped = true;
    clearTimeout(this.#loadRetry);
    document.removeEventListener('visibilitychange', this.#onVisible);
    window.removeEventListener('online', this.#onVisible);
    this.#conn.close();
  }

  #onVisible = () => {
    if (document.visibilityState === 'visible') this.#conn.reconnectNow();
  };

  /**
   * Refetches the REST side: the session itself and its proposals. A failure is retried with backoff, since nothing
   * else would fetch them again until a reconnect.
   */
  async refresh() {
    clearTimeout(this.#loadRetry);
    try {
      const [session, proposals] = await Promise.all([
        api.getSession(this.id),
        api.listProposals({ session: this.id }),
      ]);
      this.setSession(session);
      for (const proposal of proposals) this.upsertProposal(proposal);
      this.loadError = null;
      this.#loadFailures = 0;
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        this.notFound = true;
        this.stop();
        return;
      }
      if (this.#stopped) return;
      this.loadError = error instanceof Error ? error.message : String(error);
      const delay = Math.min(30_000, 1000 * 2 ** this.#loadFailures++);
      this.#loadRetry = setTimeout(() => void this.refresh(), delay);
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
    this.#settleUnconfirmed();
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
      case '_sloppod/queue': {
        const { sessionId, messages } = params as { sessionId: string; messages: QueuedMessage[] };
        if (sessionId === this.id) this.queue = messages;
        break;
      }
      case '_sloppod/proposal': {
        const { proposal } = params as { proposal: Proposal };
        if (proposal.session_id === this.id) this.upsertProposal(proposal);
        break;
      }
      case '_sloppod/permission_resolved': {
        const { sessionId, toolCallId, requestId } = params as {
          sessionId: string;
          toolCallId?: string | null;
          requestId?: RequestId | null;
        };
        if (sessionId !== this.id) break;
        // `requestId` is this connection's id for the request; questions from MCP servers may have no tool call.
        const resolved = (rpcId: RequestId, callId: string | undefined) =>
          requestId !== undefined && requestId !== null ? rpcId === requestId : callId === toolCallId;
        this.permissions = this.permissions.filter((request) => !resolved(request.rpcId, request.toolCall.toolCallId));
        this.questions = this.questions.filter((question) => !resolved(question.rpcId, question.toolCallId));
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
    if (method === 'elicitation/create') {
      const request = params as ElicitationRequest;
      if (request.sessionId === this.id) {
        if (request.mode !== 'form') {
          this.#conn.respondError(rpcId, -32602, `Unsupported elicitation mode: ${request.mode}`);
          return;
        }
        this.questions.push({ ...request, rpcId });
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
      this.proposals[index] = newerProposal(this.proposals[index], proposal);
    }
  }

  /**
   * Starts a turn. The returned promise settles with the stop reason when the turn ends. The message carries an id
   * the runtime records it under, so if the connection drops first, the next load can tell whether it arrived: if
   * not, it's sent again once, and if that fails it ends up in `undelivered`.
   */
  async prompt(text: string, id: string = crypto.randomUUID()): Promise<StopReason | undefined> {
    this.promptsInFlight++;
    try {
      const result = await this.#conn.request<{ stopReason: StopReason }>('session/prompt', {
        sessionId: this.id,
        prompt: textPrompt(text),
        _meta: { sloppod: { messageId: id } },
      });
      return result?.stopReason;
    } catch (error) {
      if (error instanceof ConnectionClosedError) this.#unconfirmed.push({ id, text });
      throw error;
    } finally {
      this.promptsInFlight--;
    }
  }

  /** Whether the runtime has a message: recorded in the transcript, or waiting in the queue. */
  received(id: string): boolean {
    return (
      this.transcript.items.some((item) => item.type === 'user' && item.messageId === id) ||
      this.queue.some((message) => message.id === id)
    );
  }

  /** After a load: messages that arrived are done; ones that didn't are sent again, once. */
  #settleUnconfirmed() {
    const unconfirmed = this.#unconfirmed.splice(0);
    for (const message of unconfirmed) {
      if (this.received(message.id)) continue;
      this.prompt(message.text, message.id).catch((error) => {
        if (error instanceof ConnectionClosedError || this.received(message.id)) return;
        this.undelivered.push(message);
      });
    }
  }

  /** Moves a queued message into the running turn. */
  steerQueued(id: string) {
    return this.#conn.request('_sloppod/steer_queued', { sessionId: this.id, id });
  }

  cancel() {
    this.#conn.notify('session/cancel', { sessionId: this.id });
    // ACP: after cancelling, the client answers its open permission requests as cancelled.
    for (const request of this.permissions) {
      this.#conn.respond(request.rpcId, { outcome: { outcome: 'cancelled' } });
    }
    for (const question of this.questions) {
      this.#conn.respond(question.rpcId, { action: 'cancel' } satisfies ElicitationResponse);
    }
    this.permissions = [];
    this.questions = [];
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

  answerQuestion(question: PendingQuestion, response: ElicitationResponse) {
    this.#conn.respond(question.rpcId, response);
    this.questions = this.questions.filter((pending) => pending.rpcId !== question.rpcId);
  }

  answerPermission(request: PendingPermission, optionId: string) {
    this.#conn.respond(request.rpcId, { outcome: { outcome: 'selected', optionId } });
    this.permissions = this.permissions.filter((pending) => pending.rpcId !== request.rpcId);
  }
}
