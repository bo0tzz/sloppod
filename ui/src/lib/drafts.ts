/**
 * What the browser keeps per session across a page reload: the message box's text, and messages sent but not yet
 * confirmed by the runtime, so a reload in between doesn't lose them. Both live in `localStorage`.
 */

export type Unconfirmed = { id: string; text: string };

const draftKey = (session: string) => `sloppod.draft.${session}`;
const unconfirmedKey = (session: string) => `sloppod.unconfirmed.${session}`;

export function loadDraft(session: string): string {
  return localStorage.getItem(draftKey(session)) ?? '';
}

export function saveDraft(session: string, text: string) {
  if (text.trim()) localStorage.setItem(draftKey(session), text);
  else localStorage.removeItem(draftKey(session));
}

export function loadUnconfirmed(session: string): Unconfirmed[] {
  try {
    const stored = JSON.parse(localStorage.getItem(unconfirmedKey(session)) ?? '[]');
    return Array.isArray(stored) ? stored.filter((m) => typeof m?.id === 'string' && typeof m?.text === 'string') : [];
  } catch {
    return [];
  }
}

export function saveUnconfirmed(session: string, messages: Unconfirmed[]) {
  if (messages.length) localStorage.setItem(unconfirmedKey(session), JSON.stringify(messages));
  else localStorage.removeItem(unconfirmedKey(session));
}
