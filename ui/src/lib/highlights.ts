import type { ToolItem, TranscriptItem } from './acp/transcript.svelte.ts';
import type { Highlight } from './api.ts';

/** What a highlight tool call is called by the agent: `mcp__sloppod__highlight`, or `highlight` where the server is dropped. */
const isHighlightCall = (item: TranscriptItem): item is ToolItem => item.type === 'tool' && /(^|__)highlight$/.test(item.title);

const RESULT_ID = /highlight ([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})/;

/** The id of the highlight a call made, from its result: `Highlighted (highlight <id>). …`. */
function resultId(item: ToolItem): string | undefined {
  for (const block of item.content) {
    const text = block.type === 'content' && block.content.type === 'text' ? block.content.text : '';
    const id = RESULT_ID.exec(text)?.[1];
    if (id) return id;
  }
  return undefined;
}

/**
 * The highlight each `highlight` tool call in the transcript made, by the call's id. The runtime's handler isn't given
 * the agent's id for the call, but its result says which highlight it made, and that is the call's. One whose result
 * doesn't (an agent that doesn't pass results on) is matched by its text: the first highlight of that text no call has
 * claimed yet, in order. A call that made none (it failed, or it's still running) has no entry, and shows as a tool call.
 */
export function matchHighlights(items: TranscriptItem[], highlights: Highlight[]): Map<string, Highlight> {
  const unclaimed = [...highlights];
  const matched = new Map<string, Highlight>();
  const calls = items.filter(isHighlightCall);
  // The ones whose result names the highlight first, so a text match can't take a highlight that's another call's.
  for (const item of calls) {
    const id = resultId(item);
    const index = id ? unclaimed.findIndex((highlight) => highlight.id === id) : -1;
    if (index !== -1) matched.set(item.toolCallId, unclaimed.splice(index, 1)[0]);
  }
  for (const item of calls) {
    if (matched.has(item.toolCallId) || resultId(item)) continue;
    const text = (item.rawInput as { text?: unknown } | undefined)?.text;
    if (typeof text !== 'string') continue;
    const index = unclaimed.findIndex((highlight) => highlight.text === text.trim());
    if (index === -1) continue;
    matched.set(item.toolCallId, unclaimed.splice(index, 1)[0]);
  }
  return matched;
}
