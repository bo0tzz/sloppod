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
import {
  api,
  ApiError,
  newerProposal,
  newerSession,
  type Highlight,
  type Proposal,
  type Session,
  type WorkDone,
} from './api.ts';
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

/**
 * Where the reader was in a session's chat, for showing it to them again where they left it (#93): the rows at the top
 * of the view by the names they have (#485), the offset the first of them was at, and whether the view was following
 * the bottom.
 */
export type ReaderPlace = { names: string[]; seq: number; offset: number; atBottom: boolean };

/** How many sessions' views are kept for switching back to (#93): the chat then shows at once, without a replay. */
const RECENT_VIEWS = 4;

/** The views of the sessions opened lately, least recently shown first. */
const recentViews = new Map<string, SessionView>();

/**
 * The view of a session: the one it had if it's among those opened lately, so switching back shows its chat at once
 * (#93), and what it missed is caught up from a load underneath.
 */
export function sessionView(id: string): SessionView {
  const kept = recentViews.get(id);
  // Only one with its chat already: a session whose load is still on its way has nothing to show, and the load in
  // flight belongs to the view that asked for it, which a new one waits out (#454).
  if (kept?.loaded) {
    // Shown again: last in the map, so the one that goes first is the one left alone longest.
    recentViews.delete(id);
    recentViews.set(id, kept);
    return kept;
  }
  const view = new SessionView(id);
  recentViews.set(id, view);
  // The oldest goes, so a few sessions' transcripts at most are held in memory.
  while (recentViews.size > RECENT_VIEWS) {
    recentViews.delete(recentViews.keys().next().value!);
  }
  return view;
}

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
/** An update with its place in the session's log (0 where it has none). */
type LoggedUpdate = { update: SessionUpdate; seq: number; at: string | null };

const READ_THROTTLE_MS = 1000;
const READ_RETRY_MS = 3000;

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
  /** The issues and pull requests the session has acted on, newest first. */
  workDone = $state<WorkDone[]>([]);
  /** What the agent highlighted for the user in this session, oldest first, the ones marked seen included (#439). */
  highlights = $state<Highlight[]>([]);
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
  #updates: LoggedUpdate[] = [];
  #replayUpdates: LoggedUpdate[] | null = null;
  /** How far this user has read, as far as this device knows: the runtime's word or what was scrolled past since. */
  #read = $state(0);
  #readSent = 0;
  #readTimer: ReturnType<typeof setTimeout> | undefined;
  /** Where to ask for older history, when the session has more than was loaded; `null` once it's all here. */
  historyBefore = $state<number | null>(null);
  loadingHistory = $state(false);
  /** Why the last page of older history didn't arrive; the reader asks for it again from there. */
  historyError = $state<string | null>(null);
  /** Messages a dropped connection cut off before the runtime confirmed them; the next load settles each. */
  #unconfirmed: Outgoing[] = [];
  /** Messages written while the connection was down, shown as waiting; the next load sends them. */
  unsent = $state<Outgoing[]>([]);
  /** Messages that couldn't be delivered, kept until the user puts each back or dismisses it. */
  undelivered = $state<Outgoing[]>([]);
  /**
   * Where the reader was in the chat, kept up to date while this view is shown and used the next time it is (#93):
   * showing the session again puts them back there rather than at the bar or the bottom.
   */
  place: ReaderPlace | null = null;

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

  /** Tries to reconnect at once, instead of waiting out the backoff. */
  retryNow() {
    app.reconnectNow(this.#consumer);
  }

  stop() {
    this.#stopped = true;
    clearTimeout(this.#loadRetry);
    this.#sendRead();
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
      const [session, proposals, highlights] = await Promise.all([
        api.getSession(this.id),
        api.listProposals({ session: this.id }),
        api.listHighlights(this.id),
      ]);
      this.setSession(session);
      // The work done is fetched once below, not for each proposal that turns out to be executed.
      this.#listing = true;
      for (const proposal of proposals) this.upsertProposal(proposal);
      this.#listing = false;
      for (const highlight of highlights) this.upsertHighlight(highlight);
      // After the proposals, so what it lists is at least as new as they are. A log beside the chat: not worth failing
      // the load for.
      await this.#fetchWorkDone().catch(() => {});
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
      this.historyError = null;
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
        if (notification.sessionId === this.id) this.#applyUpdate(notification.update, notification._meta?.sloppod?.seq, notification._meta?.sloppod?.at);
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
      case '_sloppod/highlight': {
        const { highlight } = params as { highlight: Highlight };
        if (highlight.session_id === this.id) this.upsertHighlight(highlight);
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

  #applyUpdate(update: SessionUpdate, seq = 0, at: string | null = null) {
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
    (this.#replay ?? this.transcript).apply(update, seq, at);
    (this.#replayUpdates ?? this.#updates).push({ update, seq, at });
  }

  /**
   * Puts the page of history before what's loaded in front of it. A failure is kept for the reader to ask again, since
   * nothing else would: the page is fetched by itself as they read up.
   */
  async loadEarlier() {
    const before = this.historyBefore;
    if (before === null || this.loadingHistory || this.#replay) return;
    this.loadingHistory = true;
    this.historyError = null;
    try {
      const page = await app.request<{ updates: SessionUpdate[]; at?: string[]; before: number | null }>('_sloppod/history', {
        sessionId: this.id,
        before,
      });
      // A reload meanwhile has replaced what this page goes in front of.
      if (this.historyBefore !== before || !page) return;
      const transcript = new Transcript();
      // An older page's events are before whatever was read, as far as the unread marker goes: they have no place.
      const updates = [...page.updates.map((update, index) => ({ update, seq: 0, at: page.at?.[index] ?? null })), ...this.#updates];
      for (const { update, seq, at } of updates) transcript.apply(update, seq, at);
      this.#updates = updates;
      this.transcript = transcript;
      this.historyBefore = page.before;
    } catch (error) {
      this.historyError = error instanceof Error ? error.message : String(error);
    } finally {
      this.loadingHistory = false;
    }
  }

  /** The seq of the last event read, in any of the user's UIs. */
  readSeq = $derived(Math.max(this.session?.read_seq ?? 0, this.#read));

  /** Notes that everything up to `seq` has been scrolled past. Only moves forward; reported to the runtime a moment later. */
  markRead(seq: number) {
    if (seq <= this.readSeq) return;
    this.#read = seq;
    this.#readTimer ??= setTimeout(() => this.#sendRead(), READ_THROTTLE_MS);
  }

  #sendRead() {
    clearTimeout(this.#readTimer);
    this.#readTimer = undefined;
    if (this.#read <= this.#readSent) return;
    const seq = this.#read;
    const before = this.#readSent;
    this.#readSent = seq;
    // A failure costs only a bar in the wrong place on another device, so it's tried again later, not shown.
    api.setRead(this.id, seq).catch((error) => {
      if (this.#readSent === seq) this.#readSent = before;
      if (this.#stopped || (error instanceof ApiError && error.status < 500)) return;
      this.#readTimer ??= setTimeout(() => this.#sendRead(), READ_RETRY_MS);
    });
  }

  setSession(session: Session) {
    this.session = newerSession(this.session, session);
    sessions.upsert(session);
  }

  /** Adds a highlight or updates it; once seen or retracted, a copy that says it's open (a slow fetch's) doesn't reopen it. */
  upsertHighlight(highlight: Highlight) {
    const index = this.highlights.findIndex((existing) => existing.id === highlight.id);
    if (index === -1) {
      this.highlights.push(highlight);
      this.highlights.sort((a, b) => a.created_at.localeCompare(b.created_at));
    } else if (!this.highlights[index].seen_at && !this.highlights[index].retracted_at) {
      this.highlights[index] = highlight;
    }
  }

  /** The user has seen a highlight. That's all: no turn starts and the agent isn't told. */
  async markHighlightSeen(highlight: Highlight) {
    this.upsertHighlight(await api.markHighlightSeen(this.id, highlight.id));
  }

  /** The user puts a highlight off until later (#467); the runtime brings it back at its time. No turn starts. */
  async snoozeHighlight(highlight: Highlight, until: Date) {
    this.upsertHighlight(await api.snoozeHighlight(this.id, highlight.id, until.toISOString()));
  }

  upsertProposal(proposal: Proposal) {
    const index = this.proposals.findIndex((existing) => existing.id === proposal.id);
    const before = this.proposals[index]?.status;
    if (index === -1) {
      this.proposals.push(proposal);
    } else {
      this.proposals[index] = newerProposal(this.proposals[index], proposal);
    }
    // Carried out: the item it acted on belongs in the work done.
    if (proposal.status === 'executed' && before !== 'executed' && !this.#listing) this.#fetchWorkDone().catch(() => {});
  }

  #listing = false;
  #workRequests = 0;
  #workApplied = 0;

  /** Fetches the work done. Answers can come back in any order, so one is only kept if no later request has been kept. */
  async #fetchWorkDone() {
    const request = ++this.#workRequests;
    const work = await api.listWorkDone(this.id);
    if (this.#stopped || request < this.#workApplied) return;
    this.#workApplied = request;
    this.workDone = work;
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
