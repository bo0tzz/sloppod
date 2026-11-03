import { app } from './app-connection.svelte.ts';
import { api, type Quota } from './api.ts';

/**
 * The owner's quotas for the usage meters, kept current while one is shown. The runtime pushes each change over the app's
 * connection (`_sloppod/quota`), and everything is fetched once on every (re)connect, for what changed while it was
 * down. There's no polling.
 */
class QuotaStore {
  list = $state<Quota[]>([]);

  #subscribers = 0;
  #unregister: (() => void) | undefined;
  /** Changes that came while the list was being fetched, applied again over what it returns, which may predate them. */
  #duringFetch: [string, Quota | null][] | undefined;
  /** Counts the connection's comings-up, so only the latest one's fetch is kept. */
  #generation = 0;

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
      this.#unregister = app.register({
        // Changes are pushed from the moment the connection is up, so fetching after that misses nothing. A fetch that
        // fails drops the connection, which tries again.
        onReady: async () => {
          // A fetch of an earlier connection, still on its way, is older than this one's and is dropped when it lands.
          const generation = ++this.#generation;
          const during: [string, Quota | null][] = [];
          this.#duringFetch = during;
          try {
            const list = await api.listQuotas();
            if (generation !== this.#generation) return;
            this.list = list;
            for (const [credentialId, quota] of during) this.apply(credentialId, quota);
          } finally {
            if (this.#duringFetch === during) this.#duringFetch = undefined;
          }
        },
        onNotification: (method, params) => {
          if (method !== '_sloppod/quota') return;
          const { credentialId, quota } = params as { credentialId: string; quota: Quota | null };
          this.#duringFetch?.push([credentialId, quota]);
          this.apply(credentialId, quota);
        },
      });
    }
    return () => {
      this.#subscribers--;
      if (this.#subscribers === 0) {
        this.#unregister?.();
        this.#unregister = undefined;
      }
    };
  }
}

export const quotas = new QuotaStore();
