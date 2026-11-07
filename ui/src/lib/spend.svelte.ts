import { app } from './app-connection.svelte.ts';
import { api, type Spend } from './api.ts';

/**
 * What the user's sessions on endpoint models have spent (#461), kept current while it's shown. The runtime says when
 * it changed (`_sloppod/spend`), and it's read again then and on every (re)connect. There's no polling.
 */
class SpendStore {
  spend = $state<Spend>({ total: 0, keys: [] });

  #subscribers = 0;
  #unregister: (() => void) | undefined;
  /** Counts the reads, so only the latest one's answer is kept. */
  #generation = 0;

  async #read() {
    const generation = ++this.#generation;
    const spend = await api.getSpend();
    if (generation === this.#generation) this.spend = spend;
  }

  subscribe() {
    this.#subscribers++;
    if (this.#subscribers === 1) {
      this.#unregister = app.register({
        onReady: () => this.#read(),
        onNotification: (method) => {
          if (method === '_sloppod/spend') void this.#read().catch(() => {});
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

export const spending = new SpendStore();
