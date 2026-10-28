/**
 * What the browser keeps per session across a page reload: the message box's text, and messages sent but not yet
 * confirmed by the runtime, so a reload in between doesn't lose them. Both live in `localStorage`, which only ever
 * helps: when it's full, denied or missing, everything works as before, without surviving a reload.
 */

import type { Attachment } from './images.ts';
import { readStored as read, writeStored as write } from './storage.ts';

/** A message the user sent: its id, its text and any images. */
export type Outgoing = { id: string; text: string; images?: Attachment[] };

const draftKey = (session: string) => `sloppod.draft.${session}`;
const unconfirmedKey = (session: string) => `sloppod.unconfirmed.${session}`;

export function loadDraft(session: string): string {
  return read(draftKey(session)) ?? '';
}

export function saveDraft(session: string, text: string) {
  write(draftKey(session), text.trim() ? text : null);
}

export function loadUnconfirmed(session: string): Outgoing[] {
  try {
    const stored = JSON.parse(read(unconfirmedKey(session)) ?? '[]');
    return Array.isArray(stored) ? stored.filter((m) => typeof m?.id === 'string' && typeof m?.text === 'string') : [];
  } catch {
    return [];
  }
}

export function saveUnconfirmed(session: string, messages: Outgoing[]) {
  const key = unconfirmedKey(session);
  if (!messages.length) {
    write(key, null);
    return;
  }
  // Images can be more than the browser keeps; their text is still worth keeping.
  if (!write(key, JSON.stringify(messages))) write(key, JSON.stringify(messages.map(({ images: _, ...text }) => text)));
}
