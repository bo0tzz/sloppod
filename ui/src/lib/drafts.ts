/**
 * What the browser keeps per session across a page reload: the message box's text, and messages sent but not yet
 * confirmed by the runtime, so a reload in between doesn't lose them. Both live in `localStorage`, which only ever
 * helps: when it's full, denied or missing, everything works as before, without surviving a reload.
 */

import { readStored as read, writeStored as write } from './storage.ts';

export type Unconfirmed = { id: string; text: string };

const draftKey = (session: string) => `sloppod.draft.${session}`;
const unconfirmedKey = (session: string) => `sloppod.unconfirmed.${session}`;

export function loadDraft(session: string): string {
  return read(draftKey(session)) ?? '';
}

export function saveDraft(session: string, text: string) {
  write(draftKey(session), text.trim() ? text : null);
}

export function loadUnconfirmed(session: string): Unconfirmed[] {
  try {
    const stored = JSON.parse(read(unconfirmedKey(session)) ?? '[]');
    return Array.isArray(stored) ? stored.filter((m) => typeof m?.id === 'string' && typeof m?.text === 'string') : [];
  } catch {
    return [];
  }
}

export function saveUnconfirmed(session: string, messages: Unconfirmed[]) {
  write(unconfirmedKey(session), messages.length ? JSON.stringify(messages) : null);
}
