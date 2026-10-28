import { api } from './api.ts';

/**
 * Who the runtime says we are (from the authenticating proxy), and which runtime that is. Read again whenever the
 * runtime may have been redeployed since: on a reconnect, and when the page becomes visible again.
 */
class Account {
  user = $state<string | null>(null);
  /** The runtime's version, e.g. `0.1.2-main.153`. */
  version = $state<string | null>(null);
  /** The commit or release tag the runtime was built from. */
  build = $state<string | null>(null);
  #loaded = false;
  #watching = false;
  #inFlight = false;

  /** Reads it unless it has been read already, and rereads it whenever the page becomes visible again. */
  load() {
    if (!this.#watching) {
      this.#watching = true;
      document.addEventListener('visibilitychange', () => {
        if (document.visibilityState === 'visible') this.refresh();
      });
    }
    if (!this.#loaded) this.refresh();
  }

  refresh() {
    if (this.#inFlight) return;
    this.#inFlight = true;
    api
      .me()
      .then(({ user, version, build }) => {
        this.user = user;
        this.version = version;
        this.build = build;
        this.#loaded = true;
      })
      .catch(() => {})
      .finally(() => (this.#inFlight = false));
  }
}

export const account = new Account();

const SOURCE = 'https://github.com/bo0tzz/sloppod';

/** Where to read about a build: its commit, its release for a `v…` tag, or else the repository. */
export function sourceLink(build: string | null): string {
  if (build && /^[0-9a-f]{40}$/.test(build)) return `${SOURCE}/commit/${build}`;
  if (build && /^v\d/.test(build)) return `${SOURCE}/releases/tag/${build}`;
  return SOURCE;
}
