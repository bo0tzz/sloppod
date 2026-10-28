import { api } from './api.ts';

/** Who the runtime says we are (from the authenticating proxy). Loaded once per page load. */
class Account {
  user = $state<string | null>(null);
  /** The runtime's version, e.g. `0.1.2-main.153`. */
  version = $state<string | null>(null);
  /** The commit or release tag the runtime was built from. */
  build = $state<string | null>(null);
  #requested = false;

  load() {
    if (this.#requested) return;
    this.#requested = true;
    api.me().then(
      ({ user, version, build }) => {
        this.user = user;
        this.version = version;
        this.build = build;
      },
      () => (this.#requested = false),
    );
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
