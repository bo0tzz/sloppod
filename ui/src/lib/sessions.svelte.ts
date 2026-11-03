import { AcpConnection, acpUrl } from './acp/connection.ts';
import { account } from './account.svelte.ts';
import { api, newerSession, sortSessions, type Session } from './api.ts';

/**
 * The session list, shared by the list page and the sidebar. The runtime pushes each change of a session over a
 * connection of the store's own (`_sloppod/session`), so what waits on the user shows at once. The list is also
 * fetched on a timer, for sessions that come and go (#426 is to replace it).
 */
class SessionStore {
  list = $state<Session[]>([]);
  loaded = $state(false);
  /** The connection was up and has gone: what the list says may be out of date until it's back. */
  lost = $state(false);
  error = $state<string | null>(null);
  sorted = $derived(sortSessions(this.list));

  #subscribers = 0;
  #timer: ReturnType<typeof setInterval> | undefined;
  #conn: AcpConnection | undefined;
  #everOpen = false;
  /** How many sessions were removed so far, and which, with the count when each was. */
  #removals = 0;
  #removed = new Map<string, number>();
  /** Refreshes started, and the latest one whose list was applied: an older list never replaces a newer one. */
  #refreshes = 0;
  #applied = 0;
  /**
   * What was pushed while each fetch is on its way: the list it returns may have been read before those changes (the
   * session's `updated_at` doesn't move for what it waits on), so they're applied again over it.
   */
  #pushedDuringFetch = new Set<Map<string, Session>>();

  /** A session pushed by the runtime. */
  #pushed(session: Session) {
    for (const during of this.#pushedDuringFetch) during.set(session.id, session);
    this.upsert(session);
  }

  async refresh() {
    const refresh = ++this.#refreshes;
    const removals = this.#removals;
    const pushed = new Map<string, Session>();
    this.#pushedDuringFetch.add(pushed);
    try {
      const fetched = await api.listSessions();
      if (refresh < this.#applied) return;
      this.#applied = refresh;
      const current = new Map(this.list.map((session) => [session.id, session]));
      // A list fetched before a removal may still have the session.
      this.list = fetched
        .filter((session) => (this.#removed.get(session.id) ?? 0) <= removals)
        .map((session) => newerSession(current.get(session.id), session));
      for (const [id, removal] of this.#removed) if (removal <= removals) this.#removed.delete(id);
      for (const session of pushed.values()) this.upsert(session);
      // The runtime is back after being unreachable, and may have been redeployed meanwhile.
      if (this.error) account.refresh();
      this.error = null;
    } catch (error) {
      this.error = error instanceof Error ? error.message : String(error);
    } finally {
      this.#pushedDuringFetch.delete(pushed);
      this.loaded = true;
    }
  }

  /** Adds or updates a session the list shows; a deleted one, or a review session, leaves it. */
  upsert(session: Session) {
    if (session.kind !== 'work' || session.deleted_at) {
      this.remove(session.id);
      return;
    }
    const index = this.list.findIndex((existing) => existing.id === session.id);
    if (index === -1) {
      this.list.push(session);
    } else {
      this.list[index] = newerSession(this.list[index], session);
    }
  }

  remove(id: string) {
    this.#removed.set(id, ++this.#removals);
    this.list = this.list.filter((session) => session.id !== id);
  }

  /** Keeps the list current while at least one component is subscribed. Returns the unsubscribe function. */
  subscribe(intervalMs = 5000) {
    this.#subscribers++;
    if (this.#subscribers === 1) {
      void this.refresh();
      // Changes are pushed from the moment the connection is up, so fetching after that misses nothing.
      this.#conn = new AcpConnection(acpUrl(), {
        onReady: () => this.refresh(),
        onNotification: (method, params) => {
          if (method === '_sloppod/session') this.#pushed((params as { session: Session }).session);
        },
        onRequest: (id) => this.#conn?.respondError(id, -32601, 'not handled here'),
        onStateChange: (state) => {
          if (state === 'open') {
            this.#everOpen = true;
            this.lost = false;
          } else if (this.#everOpen) {
            this.lost = true;
          }
        },
      });
      this.#conn.connect();
      this.#timer = setInterval(() => {
        if (document.visibilityState === 'visible') void this.refresh();
      }, intervalMs);
      document.addEventListener('visibilitychange', this.#onVisible);
    }
    return () => {
      this.#subscribers--;
      if (this.#subscribers === 0) {
        clearInterval(this.#timer);
        this.#conn?.close();
        this.#conn = undefined;
        this.#everOpen = false;
        this.lost = false;
        document.removeEventListener('visibilitychange', this.#onVisible);
      }
    };
  }

  #onVisible = () => {
    if (document.visibilityState === 'visible') void this.refresh();
  };
}

export const sessions = new SessionStore();
