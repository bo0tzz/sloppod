export type ConnectionState = 'connecting' | 'open' | 'closed';

export class RpcError extends Error {
  constructor(
    public code: number,
    message: string,
    public data?: unknown,
  ) {
    super(message);
  }
}

export type RequestId = number | string;

export type ConnectionHandlers = {
  /** Runs after every successful (re)connect, before the connection counts as open. Throwing drops the socket and retries. */
  onReady: (connection: AcpConnection) => Promise<void>;
  onNotification: (method: string, params: unknown) => void;
  /** Requests from the runtime; the handler must answer through `respond` or `respondError`. */
  onRequest: (id: RequestId, method: string, params: unknown) => void;
  onStateChange?: (state: ConnectionState, error?: string) => void;
};

type Pending = { resolve: (value: unknown) => void; reject: (error: Error) => void };

const MAX_BACKOFF_MS = 10_000;

/** JSON-RPC 2.0 over a WebSocket that reconnects with backoff until closed. */
export class AcpConnection {
  #url: string;
  #handlers: ConnectionHandlers;
  #ws: WebSocket | null = null;
  #nextId = 1;
  #pending = new Map<RequestId, Pending>();
  #closed = false;
  #attempt = 0;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #state: ConnectionState = 'connecting';

  constructor(url: string, handlers: ConnectionHandlers) {
    this.#url = url;
    this.#handlers = handlers;
  }

  get state() {
    return this.#state;
  }

  connect() {
    if (this.#closed) return;
    clearTimeout(this.#timer);
    this.#timer = undefined;
    this.#setState('connecting');

    const ws = new WebSocket(this.#url);
    this.#ws = ws;

    ws.onopen = async () => {
      if (ws !== this.#ws) return;
      try {
        await this.#handlers.onReady(this);
        if (ws !== this.#ws) return;
        this.#attempt = 0;
        this.#setState('open');
      } catch (error) {
        if (ws !== this.#ws) return;
        this.#setState('connecting', error instanceof Error ? error.message : String(error));
        ws.close();
      }
    };
    ws.onmessage = (event) => {
      if (ws !== this.#ws) return;
      this.#receive(event.data);
    };
    ws.onclose = () => {
      if (ws !== this.#ws) return;
      this.#ws = null;
      this.#failPending(new Error('Connection closed'));
      if (this.#closed) {
        this.#setState('closed');
      } else {
        this.#scheduleReconnect();
      }
    };
  }

  /** Skips the remaining backoff, e.g. when the page becomes visible again. */
  reconnectNow() {
    if (this.#closed || this.#ws) return;
    this.connect();
  }

  close() {
    this.#closed = true;
    clearTimeout(this.#timer);
    this.#failPending(new Error('Connection closed'));
    this.#ws?.close();
    this.#ws = null;
    this.#setState('closed');
  }

  request<T = unknown>(method: string, params: unknown): Promise<T> {
    const ws = this.#ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error('Not connected'));
    }
    const id = this.#nextId++;
    return new Promise<T>((resolve, reject) => {
      this.#pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
      ws.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }));
    });
  }

  notify(method: string, params: unknown) {
    this.#send({ jsonrpc: '2.0', method, params });
  }

  respond(id: RequestId, result: unknown) {
    this.#send({ jsonrpc: '2.0', id, result });
  }

  respondError(id: RequestId, code: number, message: string) {
    this.#send({ jsonrpc: '2.0', id, error: { code, message } });
  }

  #send(message: unknown) {
    const ws = this.#ws;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(message));
    }
  }

  #receive(data: unknown) {
    if (typeof data !== 'string') return;
    let message: {
      id?: RequestId | null;
      method?: string;
      params?: unknown;
      result?: unknown;
      error?: { code: number; message: string; data?: unknown };
    };
    try {
      message = JSON.parse(data);
    } catch {
      console.warn('acp: ignoring non-JSON frame');
      return;
    }

    if (message.method !== undefined) {
      if (message.id !== undefined && message.id !== null) {
        this.#handlers.onRequest(message.id, message.method, message.params);
      } else {
        this.#handlers.onNotification(message.method, message.params);
      }
      return;
    }

    if (message.id === undefined || message.id === null) return;
    const pending = this.#pending.get(message.id);
    if (!pending) return;
    this.#pending.delete(message.id);
    if (message.error) {
      pending.reject(new RpcError(message.error.code, message.error.message, message.error.data));
    } else {
      pending.resolve(message.result);
    }
  }

  #failPending(error: Error) {
    for (const pending of this.#pending.values()) {
      pending.reject(error);
    }
    this.#pending.clear();
  }

  #scheduleReconnect() {
    const delay = Math.min(500 * 2 ** this.#attempt, MAX_BACKOFF_MS) * (0.75 + Math.random() * 0.5);
    this.#attempt++;
    this.#setState('connecting');
    this.#timer = setTimeout(() => this.connect(), delay);
  }

  #setState(state: ConnectionState, error?: string) {
    this.#state = state;
    this.#handlers.onStateChange?.(state, error);
  }
}

export function acpUrl(): string {
  const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${protocol}//${location.host}/acp`;
}
