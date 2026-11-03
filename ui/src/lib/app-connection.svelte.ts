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
  /** Consumers whose `onReady` failed, each waiting for its next try, with how many failed so far and why. */
  #failed = new Map<Consumer, { attempts: number; error: string; timer?: ReturnType<typeof setTimeout> }>();
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
        this.#late(consumer);
      } else {
        // Coming up: the loop in `#onReady` takes it if it's still going, and the connection's 'open' does if not.
        consumer.onStateChange?.(this.state, this.error ?? undefined);
      }
    }
    return () => {
      this.#consumers.delete(consumer);
      this.#ready.delete(consumer);
      this.#forget(consumer);
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

  /** A consumer that registered on a connection that's up: it's up for it once its own state is fetched. */
  #late(consumer: Consumer) {
    consumer.onStateChange?.('connecting');
    void this.#attempt(consumer, this.#generation);
  }

  /** Runs a consumer's `onReady` and, once it's through, tells it the connection is up. */
  async #attempt(consumer: Consumer, generation: number) {
    if (!(await this.#ready_(consumer, generation))) return;
    if (generation === this.#generation && this.state === 'open' && this.#ready.has(consumer)) {
      consumer.onStateChange?.('open');
    }
  }

  /** Takes a consumer out of the failed ones, and stops its waiting to try again. */
  #forget(consumer: Consumer) {
    clearTimeout(this.#failed.get(consumer)?.timer);
    this.#failed.delete(consumer);
  }

  /**
   * A consumer's own fetch failed: it tries again after a backoff, like the session's REST side does. The connection
   * and the other consumers are left as they are; only a failure of the connection itself reconnects.
   */
  #retryLater(consumer: Consumer, generation: number, error: unknown) {
    const message = error instanceof Error ? error.message : String(error);
    const attempts = (this.#failed.get(consumer)?.attempts ?? 0) + 1;
    clearTimeout(this.#failed.get(consumer)?.timer);
    const timer = setTimeout(() => {
      if (generation !== this.#generation || this.state !== 'open' || !this.#consumers.has(consumer)) return;
      void this.#attempt(consumer, generation);
    }, Math.min(30_000, 1000 * 2 ** (attempts - 1)));
    this.#failed.set(consumer, { attempts, error: message, timer });
    consumer.onStateChange?.('connecting', message);
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
          for (const consumer of [...this.#failed.keys()]) this.#forget(consumer);
        }
        for (const consumer of [...this.#consumers]) {
          const failure = this.#failed.get(consumer);
          // One whose fetch failed is still waiting to try again; the connection being up isn't news to it.
          if (state === 'open' && failure) consumer.onStateChange?.('connecting', failure.error);
          // One that registered after the loop in `#onReady` was done with its snapshot hasn't been readied.
          else if (state === 'open' && !this.#ready.has(consumer)) this.#late(consumer);
          else consumer.onStateChange?.(state, error);
        }
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
    for (const consumer of [...this.#failed.keys()]) this.#forget(consumer);
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
    // One after the other. A consumer whose fetch fails doesn't stop the rest, or the connection: it tries again on
    // its own. Those that register meanwhile are taken in turn: it goes on until none is left.
    for (;;) {
      if (generation !== this.#generation) return;
      const next = [...this.#consumers].find((consumer) => !this.#ready.has(consumer) && !this.#failed.has(consumer));
      if (!next) return;
      await this.#ready_(next, generation);
    }
  }

  /**
   * Runs a consumer's `onReady` once on this connection; false if it didn't get that far: it left, or its fetch
   * failed and is tried again later.
   */
  async #ready_(consumer: Consumer, generation: number): Promise<boolean> {
    // One that left while it waited its turn isn't started.
    if (this.#ready.has(consumer) || !this.#consumers.has(consumer)) return false;
    this.#ready.add(consumer);
    try {
      await consumer.onReady(this);
      this.#forget(consumer);
      return true;
    } catch (error) {
      this.#ready.delete(consumer);
      // The connection moving on is its own failure, and takes everything up again.
      if (generation === this.#generation && this.#consumers.has(consumer)) this.#retryLater(consumer, generation, error);
      return false;
    }
  }
}

export const app = new AppConnection();
export type { AppConnection };
