import { AcpConnection, acpUrl } from './acp/connection.ts';
import { api, type Quota } from './api.ts';

/**
 * The owner's quotas for the usage meters, kept current while one is shown. The runtime pushes each change over a
 * connection of the store's own (`_sloppod/quota`), and everything is fetched once on every (re)connect, for what
 * changed while it was down. There's no polling.
 */
class QuotaStore {
  list = $state<Quota[]>([]);

  #subscribers = 0;
  #conn: AcpConnection | undefined;
  /** Changes that came while the list was being fetched, applied again over what it returns, which may predate them. */
  #duringFetch: [string, Quota | null][] | undefined;
  // A socket that died while the tab was hidden or offline comes back without waiting out its backoff.
  #wake = () => {
    if (document.visibilityState === 'visible') this.#conn?.reconnectNow();
  };

  /** A change pushed by the runtime: the credential's quota as it is now, or `null` once the credential is gone. */
  apply(credentialId: string, quota: Quota | null) {
    const others = this.list.filter((q) => q.credential_id !== credentialId);
    if (!quota) {
      this.list = others;
      return;
    }
    const index = this.list.findIndex((q) => q.credential_id === credentialId);
    // In place keeps the meters in the order the runtime lists them; a new one goes where it sorts.
    if (index >= 0) this.list = this.list.map((q, i) => (i === index ? quota : q));
    else this.list = [...others, quota].sort((a, b) => a.kind.localeCompare(b.kind) || a.name.localeCompare(b.name));
  }

  subscribe() {
    this.#subscribers++;
    if (this.#subscribers === 1) {
      this.#conn = new AcpConnection(acpUrl(), {
        // Changes are pushed from the moment the connection is up, so fetching after that misses nothing. A fetch that
        // fails drops the connection, which tries again.
        onReady: async () => {
          this.#duringFetch = [];
          try {
            const list = await api.listQuotas();
            this.list = list;
            for (const [credentialId, quota] of this.#duringFetch) this.apply(credentialId, quota);
          } finally {
            this.#duringFetch = undefined;
          }
        },
        onNotification: (method, params) => {
          if (method !== '_sloppod/quota') return;
          const { credentialId, quota } = params as { credentialId: string; quota: Quota | null };
          this.#duringFetch?.push([credentialId, quota]);
          this.apply(credentialId, quota);
        },
        onRequest: (id) => this.#conn?.respondError(id, -32601, 'not handled here'),
      });
      this.#conn.connect();
      document.addEventListener('visibilitychange', this.#wake);
      window.addEventListener('online', this.#wake);
    }
    return () => {
      this.#subscribers--;
      if (this.#subscribers === 0) {
        this.#conn?.close();
        this.#conn = undefined;
        document.removeEventListener('visibilitychange', this.#wake);
        window.removeEventListener('online', this.#wake);
      }
    };
  }
}

export const quotas = new QuotaStore();
