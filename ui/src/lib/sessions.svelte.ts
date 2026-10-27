import { api, newerSession, sortSessions, type Session } from './api.ts';

/** The session list, shared by the list page and the sidebar. The runtime has no list-wide push, so it polls. */
class SessionStore {
  list = $state<Session[]>([]);
  loaded = $state(false);
  error = $state<string | null>(null);
  sorted = $derived(sortSessions(this.list));

  #subscribers = 0;
  #timer: ReturnType<typeof setInterval> | undefined;
  /** How many sessions were removed so far, and which, with the count when each was. */
  #removals = 0;
  #removed = new Map<string, number>();

  async refresh() {
    const removals = this.#removals;
    try {
      const fetched = await api.listSessions();
      const current = new Map(this.list.map((session) => [session.id, session]));
      // A list fetched before a removal may still have the session.
      this.list = fetched
        .filter((session) => (this.#removed.get(session.id) ?? 0) <= removals)
        .map((session) => newerSession(current.get(session.id), session));
      for (const [id, removal] of this.#removed) if (removal <= removals) this.#removed.delete(id);
      this.error = null;
    } catch (error) {
      this.error = error instanceof Error ? error.message : String(error);
    } finally {
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

  /** Polls while at least one component is subscribed. Returns the unsubscribe function. */
  subscribe(intervalMs = 5000) {
    this.#subscribers++;
    if (this.#subscribers === 1) {
      void this.refresh();
      this.#timer = setInterval(() => {
        if (document.visibilityState === 'visible') void this.refresh();
      }, intervalMs);
      document.addEventListener('visibilitychange', this.#onVisible);
    }
    return () => {
      this.#subscribers--;
      if (this.#subscribers === 0) {
        clearInterval(this.#timer);
        document.removeEventListener('visibilitychange', this.#onVisible);
      }
    };
  }

  #onVisible = () => {
    if (document.visibilityState === 'visible') void this.refresh();
  };
}

export const sessions = new SessionStore();
