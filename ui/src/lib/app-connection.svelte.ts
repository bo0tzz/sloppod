import { AcpConnection, acpUrl, type ConnectionState, type RequestId } from './acp/connection.ts';
import type { InitializeResponse } from './acp/types.ts';
import { account } from './account.svelte.ts';

/** What shares the app's connection: the session list, the usage meters, an open session. */
export type Consumer = {
  /**
   * The session it shows, if it shows one: the runtime's requests for that session (permissions, questions) come to
   * it, and no other.
   */
  sessionId?: string;
  /** Runs after every (re)connect, and when it registers on a connection that's up: fetches its state once. */
  onReady: (connection: AppConnection) => Promise<void>;
  onNotification?: (method: string, params: unknown) => void;
  onRequest?: (id: RequestId, method: string, params: unknown) => void;
  /** The connection's state, as it changes, and once on registering. */
  onStateChange?: (state: ConnectionState, error?: string) => void;
};

/**
 * The one `/acp` connection of the page, shared by everything that follows the runtime: it's opened when the first
 * consumer registers and closed with the last. Notifications go to every consumer, which keeps what concerns it; a
 * request for a session goes to the consumer showing it. Each consumer fetches its state once on every (re)connect,
 * so nothing polls: whatever changed while the connection was down is read then, and what changes after is pushed.
 */
class AppConnection {
  state = $state<ConnectionState>('closed');
  error = $state<string | null>(null);
  steeringSupported = $state(false);
  imagesSupported = $state(false);

  #consumers = new Set<Consumer>();
  #conn: AcpConnection | undefined;
  /** Consumers whose `onReady` ran on the connection as it is now. */
  #ready = new Set<Consumer>();
  /** Counts the connection's comings-up; one that's been replaced stops what it was doing. */
  #generation = 0;
  #initialized = false;
  #connectedBefore = false;
  // A socket that died while the tab was hidden or offline comes back without waiting out its backoff.
  #wake = () => {
    if (document.visibilityState === 'visible') this.#conn?.reconnectNow();
  };

  register(consumer: Consumer): () => void {
    this.#consumers.add(consumer);
    if (!this.#conn) {
      this.#open();
    } else {
      if (this.#initialized && this.state === 'open') {
        // Up already: it's up for this consumer once its own state is fetched.
        const generation = this.#generation;
        consumer.onStateChange?.('connecting');
        void this.#ready_(consumer, generation).then(() => {
          if (generation === this.#generation && this.state === 'open' && this.#ready.has(consumer)) {
            consumer.onStateChange?.('open');
          }
        });
      } else {
        consumer.onStateChange?.(this.state, this.error ?? undefined);
      }
    }
    return () => {
      this.#consumers.delete(consumer);
      this.#ready.delete(consumer);
      if (this.#consumers.size === 0) this.#close();
    };
  }

  request<T = unknown>(method: string, params: unknown): Promise<T> {
    if (!this.#conn) return Promise.reject(new Error('Not connected'));
    return this.#conn.request<T>(method, params);
  }

  notify(method: string, params: unknown) {
    this.#conn?.notify(method, params);
  }

  respond(id: RequestId, result: unknown) {
    this.#conn?.respond(id, result);
  }

  respondError(id: RequestId, code: number, message: string) {
    this.#conn?.respondError(id, code, message);
  }

  #open() {
    this.#conn = new AcpConnection(acpUrl(), {
      onReady: (connection) => this.#onReady(connection),
      onNotification: (method, params) => {
        for (const consumer of [...this.#consumers]) consumer.onNotification?.(method, params);
      },
      onRequest: (id, method, params) => {
        const sessionId = (params as { sessionId?: string } | undefined)?.sessionId;
        const consumer = [...this.#consumers].find((c) => c.sessionId !== undefined && c.sessionId === sessionId);
        if (consumer?.onRequest) consumer.onRequest(id, method, params);
        else this.#conn?.respondError(id, -32601, `Method not found: ${method}`);
      },
      onStateChange: (state, error) => {
        this.state = state;
        this.error = error ?? null;
        if (state !== 'open') {
          this.#initialized = false;
          this.#ready.clear();
        }
        for (const consumer of [...this.#consumers]) consumer.onStateChange?.(state, error);
      },
    });
    this.#conn.connect();
    document.addEventListener('visibilitychange', this.#wake);
    window.addEventListener('online', this.#wake);
  }

  #close() {
    this.#generation++;
    this.#conn?.close();
    this.#conn = undefined;
    this.#initialized = false;
    this.#connectedBefore = false;
    this.#ready.clear();
    this.state = 'closed';
    document.removeEventListener('visibilitychange', this.#wake);
    window.removeEventListener('online', this.#wake);
  }

  async #onReady(connection: AcpConnection) {
    const generation = ++this.#generation;
    const init = await connection.request<InitializeResponse>('initialize', {
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
      clientInfo: { name: 'sloppod-ui', version: '0.0.0' },
    });
    this.steeringSupported = init?._meta?.steering?.supported === true;
    this.imagesSupported = init?.agentCapabilities?.promptCapabilities?.image === true;
    // A reconnect: the runtime may have been redeployed meanwhile.
    if (this.#connectedBefore) account.refresh();
    this.#connectedBefore = true;
    this.#initialized = true;
    // One after the other, as a session's load must not overtake the list's fetch; one failing drops the socket, which
    // comes back and tries them all again.
    for (const consumer of [...this.#consumers]) {
      if (generation !== this.#generation) return;
      await this.#ready_(consumer, generation, true);
    }
  }

  /** Runs a consumer's `onReady` once on this connection. A failure drops the socket, as `onReady` throwing does. */
  async #ready_(consumer: Consumer, generation: number, throws = false) {
    if (this.#ready.has(consumer)) return;
    this.#ready.add(consumer);
    try {
      await consumer.onReady(this);
    } catch (error) {
      this.#ready.delete(consumer);
      if (throws) throw error;
      if (generation === this.#generation) this.#conn?.drop(error instanceof Error ? error.message : String(error));
    }
  }
}

export const app = new AppConnection();
export type { AppConnection };
