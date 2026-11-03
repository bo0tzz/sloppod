import { ConnectionClosedError, type ConnectionState, type RequestId } from './acp/connection.ts';
import { Transcript } from './acp/transcript.svelte.ts';
import type {
  ContentBlock,
  LoadSessionResponse,
  ElicitationRequest,
  ElicitationResponse,
  RequestPermissionRequest,
  SessionConfigOption,
  SessionNotification,
  SessionUpdate,
  StopReason,
} from './acp/types.ts';
import { app, type Consumer } from './app-connection.svelte.ts';
import { api, ApiError, newerProposal, newerSession, type Proposal, type Session } from './api.ts';
import { loadUnconfirmed, saveAnswer, saveUnconfirmed, type Outgoing } from './drafts.ts';
import { sessions } from './sessions.svelte.ts';

export type PendingPermission = RequestPermissionRequest & { rpcId: RequestId };
/**
 * A question from the agent. `stale` while the connection is down: it can't be answered then, and the runtime sends
 * it again once the session has loaded, as a new request that takes its place.
 */
export type PendingQuestion = ElicitationRequest & {
  rpcId: RequestId;
  stale?: boolean;
  /** What tells it apart: see `questionKey`. */
  key: string;
  /** Whether `key` stays the same when it's sent again, so it can be matched and its answer kept across reloads. */
  stable: boolean;
};

/** Numbers questions that have no identity of their own, uniquely for as long as the page lives. */
let unidentified = 0;

/**
 * What tells a question apart, across reconnects too: an `ask_user` question's own id, or the agent's tool call. A
 * question with neither gets a key of its own for this page, never matched with another; request ids can't serve,
 * since every connection numbers them from 1.
 */
export function questionKey(question: ElicitationRequest): { key: string; stable: boolean } {
  const stable = question._meta?.sloppod?.questionId ?? question.toolCallId;
  return stable ? { key: stable, stable: true } : { key: `unidentified:${++unidentified}`, stable: false };
}

/** How long a question shown while reconnecting waits to be sent again before it's taken down. */
const STALE_QUESTION_GRACE_MS = 10_000;
/** A message waiting for the running turn to end (`_sloppod/queue`). */
export type QueuedMessage = { id: string; prompt: ContentBlock[] };

export const queuedText = (message: QueuedMessage) =>
  message.prompt.map((block) => (block.type === 'text' ? block.text : '')).join('');

/** A message as the prompt it's sent as: its text, then its images. */
const promptBlocks = ({ text, images = [] }: Outgoing): ContentBlock[] => [
  ...(text ? [{ type: 'text' as const, text }] : []),
  ...images.map((image) => ({ type: 'image' as const, ...image })),
];

/**
 * The sessions being closed on the app's connection, by id: what a view coming back to one waits for, or its load could
 * be undone by the close of the one before.
 */
const closing = new Map<string, Promise<unknown>>();

/** Puts `done` into the barrier for `id`, after whatever it already holds, and takes it out once it's the last. */
function reserve(id: string, done: Promise<unknown>) {
  const barrier = Promise.all([closing.get(id), done]).finally(() => {
    if (closing.get(id) === barrier) closing.delete(id);
  });
  closing.set(id, barrier);
}

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
  /** True once `proposals` is what the runtime listed, so counts taken from it can be trusted. */
  proposalsLoaded = $state(false);
  #loadFailures = 0;
  /** Refreshes started, and the latest one that succeeded: a failure of an older one than that isn't an error to show. */
  #refreshes = 0;
  #refreshed = 0;
  #loadRetry: ReturnType<typeof setTimeout> | undefined;
  #stopped = false;
  /** A `session/load` is on its way over the shared connection. */
  #loading = false;
  /** Lifts the barrier a stop put up for a load that was still on its way. */
  #lifted: (() => void) | undefined;

  configOptions = $state<SessionConfigOption[]>([]);
  /** The agent's model picker, if it offers one. */
  model = $derived(this.configOptions.find((option) => option.id === 'model' && option.type === 'select'));
  get steeringSupported() {
    return app.steeringSupported;
  }
  /** Whether the agent takes images in a prompt. */
  get imagesSupported() {
    return app.imagesSupported;
  }

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

  /** What this session registers on the app's connection while it's shown. */
  #consumer: Consumer;
  #unregister: (() => void) | undefined;
  /** Receives the history replay of a (re)load, and replaces `transcript` once the load completes. */
  #replay: Transcript | null = null;
  /** The updates `transcript` is built from, oldest first, so older history can go in front of them. */
  #updates: SessionUpdate[] = [];
  #replayUpdates: SessionUpdate[] | null = null;
  /** Where to ask for older history, when the session has more than was loaded; `null` once it's all here. */
  historyBefore = $state<number | null>(null);
  loadingHistory = $state(false);
  /** Messages a dropped connection cut off before the runtime confirmed them; the next load settles each. */
  #unconfirmed: Outgoing[] = [];
  /** Messages written while the connection was down, shown as waiting; the next load sends them. */
  unsent = $state<Outgoing[]>([]);
  /** Messages that couldn't be delivered, kept until the user puts each back or dismisses it. */
  undelivered = $state<Outgoing[]>([]);

  constructor(id: string) {
    this.id = id;
    // Sent before a reload and never confirmed: settled like any other once the session loads.
    this.#unconfirmed = loadUnconfirmed(id);
    this.#consumer = {
      sessionId: id,
      onReady: () => this.#onReady(),
      onNotification: (method, params) => this.#onNotification(method, params),
      onRequest: (rpcId, method, params) => this.#onRequest(rpcId, method, params),
      onStateChange: (state, error) => {
        this.connection = state;
        this.connectionError = error ?? null;
        // Once the connection is fully up: messages cut off by the last drop, or written while it was down.
        if (state === 'open' && this.loaded) this.#settleUnconfirmed();
        if (state === 'open') this.#dropStaleQuestionsLater();
        if (state !== 'open') {
          // Requests from a dropped socket can no longer be answered; a reload re-sends live ones. Questions stay
          // shown meanwhile, so the answer being written isn't lost from view.
          this.permissions = [];
          this.questions = this.questions.map((question) => ({ ...question, stale: true }));
        }
      },
    };
  }

  start() {
    this.#stopped = false;
    void this.refresh();
    this.#unregister = app.register(this.#consumer);
  }

  stop() {
    this.#stopped = true;
    clearTimeout(this.#loadRetry);
    this.#unregister?.();
    this.#unregister = undefined;
    // The runtime stops sending this session's events and requests on the shared connection; what it left open waits.
    // A load still on its way ends with the close instead, or it would install what the close removed. Meanwhile
    // the barrier is up, so a view that comes back to the session waits for both.
    if (this.#loading) {
      reserve(this.id, new Promise<void>((resolve) => (this.#lifted = resolve)));
    } else if (this.loaded) {
      void this.#close();
    }
  }

  /** Tells the runtime this view is done with the session, and holds the barrier until it has. */
  #close(): Promise<void> {
    const closed = app
      .request('session/close', { sessionId: this.id })
      .then(() => {}, () => {});
    reserve(this.id, closed);
    return closed;
  }

  /**
   * Refetches the REST side: the session itself and its proposals. A failure is retried with backoff, since nothing
   * else would fetch them again until a reconnect.
   */
  async refresh() {
    if (this.#stopped) return;
    clearTimeout(this.#loadRetry);
    const refresh = ++this.#refreshes;
    try {
      const [session, proposals] = await Promise.all([
        api.getSession(this.id),
        api.listProposals({ session: this.id }),
      ]);
      this.setSession(session);
      for (const proposal of proposals) this.upsertProposal(proposal);
      this.proposalsLoaded = true;
      this.loadError = null;
      this.#loadFailures = 0;
      this.#refreshed = Math.max(this.#refreshed, refresh);
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        this.notFound = true;
        this.stop();
        return;
      }
      // Overlapping refreshes (the one after `session/load`, a reconnect) can come back in any order; a late failure
      // of one that a newer refresh has already made up for would show an error over what has loaded.
      if (this.#stopped || refresh < this.#refreshed) return;
      this.loadError = error instanceof Error ? error.message : String(error);
      // Overlapping refreshes (a reconnect during a retry) can both fail; only one retry stays scheduled.
      clearTimeout(this.#loadRetry);
      const delay = Math.min(30_000, 1000 * 2 ** this.#loadFailures++);
      this.#loadRetry = setTimeout(() => void this.refresh(), delay);
    }
  }

  async #onReady() {
    // Back on a session just left: its close has to be through, or it could land after this load and end it.
    await closing.get(this.id);
    if (this.#stopped) return;
    this.#replay = new Transcript();
    this.#replayUpdates = [];
    this.#loading = true;
    try {
      const response = await app.request<LoadSessionResponse>('session/load', {
        sessionId: this.id,
        cwd: '/',
        mcpServers: [],
      });
      // The view was left while the runtime read the session: it has subscribed this socket by now, and stops.
      if (this.#stopped) {
        await this.#close();
        return;
      }
      this.transcript = this.#replay;
      this.#updates = this.#replayUpdates;
      this.historyBefore = (response?._meta?.sloppod as { historyBefore?: number | null } | undefined)?.historyBefore ?? null;
      this.configOptions = response?.configOptions ?? [];
    } finally {
      this.#loading = false;
      this.#lifted?.();
      this.#lifted = undefined;
      this.#replay = null;
      this.#replayUpdates = null;
    }
    this.loaded = true;
    // Fetched again only now that `session/load` has subscribed this socket: a proposal made since the last fetch
    // then comes one way or the other, where fetching alongside the load could miss it (#423).
    await this.refresh();
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
        this.#closeQuestions((question) => resolved(question.rpcId, question.toolCallId));
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
          app.respondError(rpcId, -32602, `Unsupported elicitation mode: ${request.mode}`);
          return;
        }
        const pending = { ...request, rpcId, ...questionKey(request) };
        this.questions = [
          ...this.questions.filter((question) => !(pending.stable && question.key === pending.key)),
          pending,
        ];
        return;
      }
    }
    app.respondError(rpcId, -32601, `Method not found: ${method}`);
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
    (this.#replayUpdates ?? this.#updates).push(update);
  }

  /** Puts the page of history before what's loaded in front of it. */
  async loadEarlier() {
    const before = this.historyBefore;
    if (before === null || this.loadingHistory || this.#replay) return;
    this.loadingHistory = true;
    try {
      const page = await app.request<{ updates: SessionUpdate[]; before: number | null }>('_sloppod/history', {
        sessionId: this.id,
        before,
      });
      // A reload meanwhile has replaced what this page goes in front of.
      if (this.historyBefore !== before || !page) return;
      const transcript = new Transcript();
      const updates = [...page.updates, ...this.#updates];
      for (const update of updates) transcript.apply(update);
      this.#updates = updates;
      this.transcript = transcript;
      this.historyBefore = page.before;
    } finally {
      this.loadingHistory = false;
    }
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
  async prompt(message: Outgoing): Promise<StopReason | undefined> {
    const { id } = message;
    this.promptsInFlight++;
    // Kept until the runtime answers, so a page reload before then can still tell whether it arrived.
    this.#keepSending(message);
    try {
      const result = await app.request<{ stopReason: StopReason }>('session/prompt', {
        sessionId: this.id,
        prompt: promptBlocks(message),
        _meta: { sloppod: { messageId: id } },
      });
      this.#doneSending(id);
      return result?.stopReason;
    } catch (error) {
      if (error instanceof ConnectionClosedError) this.#unconfirmed.push(message);
      else this.#doneSending(id);
      throw error;
    } finally {
      this.promptsInFlight--;
    }
  }

  /**
   * Keeps a message written while the connection is down and sends it once the session has loaded again, the same
   * way as one a dropped connection cut off: under its id, so it can't arrive twice.
   */
  sendWhenConnected(message: Outgoing) {
    this.#keepSending(message);
    this.#unconfirmed.push(message);
    this.unsent.push(message);
  }

  #keepSending(message: Outgoing) {
    const kept = loadUnconfirmed(this.id).filter((kept) => kept.id !== message.id);
    saveUnconfirmed(this.id, [...kept, message]);
  }

  #doneSending(id: string) {
    saveUnconfirmed(
      this.id,
      loadUnconfirmed(this.id).filter((message) => message.id !== id),
    );
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
    this.unsent = this.unsent.filter((message) => !unconfirmed.some((settled) => settled.id === message.id));
    for (const message of unconfirmed) {
      if (this.received(message.id)) {
        this.#doneSending(message.id);
        continue;
      }
      this.prompt(message).catch((error) => {
        if (error instanceof ConnectionClosedError || this.received(message.id)) return;
        this.undelivered.push(message);
      });
    }
  }

  /** Moves a queued message into the running turn. */
  steerQueued(id: string) {
    return app.request('_sloppod/steer_queued', { sessionId: this.id, id });
  }

  /** Takes a queued message out of the queue without running it; resolves with what it held. */
  async removeQueued(id: string): Promise<ContentBlock[]> {
    const { prompt } = await app.request<{ prompt: ContentBlock[] }>('_sloppod/remove_queued', {
      sessionId: this.id,
      id,
    });
    this.#doneSending(id);
    return prompt;
  }

  /**
   * Stops the turn. The runtime answers the turn's open requests as cancelled and tells every UI they're resolved;
   * questions asked with `ask_user` aren't the turn's, so they stay open.
   */
  cancel() {
    app.notify('session/cancel', { sessionId: this.id });
  }

  async setConfigOption(configId: string, value: string) {
    const previous = this.configOptions;
    // Show the choice right away; the response carries the authoritative options.
    this.configOptions = previous.map((option) =>
      option.id === configId && option.type === 'select' ? { ...option, currentValue: value } : option,
    );
    try {
      const response = await app.request<{ configOptions?: SessionConfigOption[] }>('session/set_config_option', {
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

  /** Questions the runtime didn't send again after a reconnect are no longer open, e.g. answered elsewhere. */
  #dropStaleQuestionsLater() {
    setTimeout(() => {
      if (this.connection === 'open') this.#closeQuestions((question) => question.stale === true);
    }, STALE_QUESTION_GRACE_MS);
  }

  /** Takes down questions that are no longer open (answered elsewhere, say), with the answers kept for them. */
  #closeQuestions(closed: (question: PendingQuestion) => boolean) {
    for (const question of this.questions.filter(closed)) if (question.stable) saveAnswer(this.id, question.key, null);
    this.questions = this.questions.filter((question) => !closed(question));
  }

  answerQuestion(question: PendingQuestion, response: ElicitationResponse) {
    if (question.stale) return;
    app.respond(question.rpcId, response);
    this.questions = this.questions.filter((pending) => pending.rpcId !== question.rpcId);
  }

  answerPermission(request: PendingPermission, optionId: string) {
    app.respond(request.rpcId, { outcome: { outcome: 'selected', optionId } });
    this.permissions = this.permissions.filter((pending) => pending.rpcId !== request.rpcId);
  }
}
