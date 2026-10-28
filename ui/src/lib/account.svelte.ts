import { api } from './api.ts';

/** Who the runtime says we are (from the authenticating proxy). Loaded once per page load. */
class Account {
  user = $state<string | null>(null);
  /** What the runtime was built from, e.g. a release tag. */
  build = $state<string | null>(null);
  #requested = false;

  load() {
    if (this.#requested) return;
    this.#requested = true;
    api.me().then(
      ({ user, build }) => {
        this.user = user;
        this.build = build;
      },
      () => (this.#requested = false),
    );
  }
}

export const account = new Account();
