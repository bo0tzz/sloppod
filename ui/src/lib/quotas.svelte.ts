import { api, type Quota } from './api.ts';

/** The owner's quotas for the sidebar's meters, polled while one is shown. They change with use, not by the second. */
class QuotaStore {
  list = $state<Quota[]>([]);

  #subscribers = 0;
  #timer: ReturnType<typeof setInterval> | undefined;

  async refresh() {
    try {
      this.list = await api.listQuotas();
    } catch {
      // Keep the last known quotas; the next poll tries again.
    }
  }

  subscribe(intervalMs = 60_000) {
    this.#subscribers++;
    if (this.#subscribers === 1) {
      void this.refresh();
      this.#timer = setInterval(() => {
        if (document.visibilityState === 'visible') void this.refresh();
      }, intervalMs);
    }
    return () => {
      this.#subscribers--;
      if (this.#subscribers === 0) clearInterval(this.#timer);
    };
  }
}

export const quotas = new QuotaStore();
