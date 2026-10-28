import type { TranscriptItem } from './acp/transcript.svelte.ts';
import { readStored, writeStored } from './storage.ts';

/** An id that names the item across page loads, if it has one. */
function itemId(item: TranscriptItem): string | null {
  switch (item.type) {
    case 'user':
    case 'agent':
    case 'thought':
      return item.messageId;
    case 'tool':
      return item.toolCallId;
    case 'compaction':
      return item.compactionId;
    default:
      return null;
  }
}

/** The last item with an id: anything after it without one goes unmarked until something with an id follows. */
function lastId(items: TranscriptItem[]): string | null {
  for (let i = items.length - 1; i >= 0; i--) {
    const id = itemId(items[i]);
    if (id) return id;
  }
  return null;
}

/**
 * How far the reader has read a session's transcript, kept across page loads, and where the bar marking it goes. The
 * reader has read everything shown while the page is visible and the transcript is scrolled to the bottom. The bar
 * stays where they had read up to when they arrived or came back to the page, until they send something.
 */
export class ReadMarker {
  #key: string;
  #readUpTo: string | null;
  #barAfter = $state<string | null>(null);

  constructor(session: string) {
    this.#key = `sloppod.read.${session}`;
    this.#readUpTo = readStored(this.#key);
    this.#barAfter = this.#readUpTo;
  }

  /** The key of the first item the reader hadn't read, which the bar goes in front of. */
  unreadFrom(items: TranscriptItem[]): string | null {
    if (!this.#barAfter) return null;
    const index = items.findIndex((item) => itemId(item) === this.#barAfter);
    return index === -1 ? null : (items[index + 1]?.key ?? null);
  }

  read(items: TranscriptItem[]) {
    const id = lastId(items);
    if (!id || id === this.#readUpTo) return;
    this.#readUpTo = id;
    writeStored(this.#key, id);
  }

  /** Moves the bar to what's read so far, for a reader coming back. */
  returned() {
    this.#barAfter = this.#readUpTo;
  }

  /** Takes the bar away: the reader has caught up. */
  clear() {
    this.#barAfter = null;
  }
}
