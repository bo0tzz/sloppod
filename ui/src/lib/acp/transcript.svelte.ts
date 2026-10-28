import { SvelteMap } from 'svelte/reactivity';
import type {
  ContentBlock,
  ContentChunk,
  PlanEntry,
  SessionUpdate,
  ToolCall,
  ToolCallContent,
  ToolCallLocation,
  ToolCallStatus,
  ToolCallUpdate,
  ToolKind,
} from './types.ts';

export type MessagePart =
  | { type: 'text'; text: string }
  | { type: 'image'; src: string }
  | { type: 'link'; uri: string; name: string };

export type MessageItem = {
  type: 'user' | 'agent' | 'thought';
  key: string;
  messageId: string | null;
  /** A user message delivered into a running turn (the runtime marks the echo with `_meta.sloppod.steering`). */
  steering: boolean;
  /** A message from sloppod itself (`_meta.sloppod.notice`), e.g. that a proposal was decided. */
  notice: boolean;
  parts: MessagePart[];
};

export type ToolItem = {
  type: 'tool';
  key: string;
  toolCallId: string;
  title: string;
  kind: ToolKind;
  status: ToolCallStatus;
  content: ToolCallContent[];
  locations: ToolCallLocation[];
  rawInput?: unknown;
  /** Output the runtime delivered through the `_meta.terminal_output` extension, by terminal ID. */
  terminals: Record<string, string>;
};

export type PlanItem = { type: 'plan'; key: string; entries: PlanEntry[] };

export type CompactionItem = {
  type: 'compaction';
  key: string;
  compactionId: string;
  status: string;
  summary: string;
  error?: string | null;
};

export type NoticeItem = { type: 'notice'; key: string; severity: string; title: string; description?: string | null };

export type TranscriptItem = MessageItem | ToolItem | PlanItem | CompactionItem | NoticeItem;

const chunkTypes = {
  user_message_chunk: 'user',
  agent_message_chunk: 'agent',
  agent_thought_chunk: 'thought',
} as const;

function blockToPart(block: ContentBlock): MessagePart | null {
  switch (block.type) {
    case 'text':
      return { type: 'text', text: block.text };
    case 'image':
      return { type: 'image', src: block.uri ?? `data:${block.mimeType};base64,${block.data}` };
    case 'resource_link':
      return { type: 'link', uri: block.uri, name: block.title ?? block.name };
    case 'resource':
      return block.resource.text === undefined
        ? { type: 'link', uri: block.resource.uri, name: block.resource.uri }
        : { type: 'text', text: block.resource.text };
    default:
      return null;
  }
}

/** A message as the Markdown it arrived as; images have no Markdown form and are left out. */
export function messageMarkdown(message: MessageItem): string {
  return message.parts
    .flatMap((part) => {
      if (part.type === 'text') return [part.text];
      if (part.type === 'link') return [`[${part.name}](${part.uri})`];
      return [];
    })
    .join('\n\n');
}

export function blockText(block: ContentBlock): string {
  const part = blockToPart(block);
  return part?.type === 'text' ? part.text : '';
}

/** Folds `session/update` notifications into a list of renderable items. */
export class Transcript {
  items = $state<TranscriptItem[]>([]);

  #seq = 0;
  // Reactive, so a permission card that looked its tool call up before it arrived picks it up.
  #tools = new SvelteMap<string, ToolItem>();
  #compactions = new Map<string, CompactionItem>();
  #plan: PlanItem | null = null;

  apply(update: SessionUpdate) {
    switch (update.sessionUpdate) {
      case 'user_message_chunk':
      case 'agent_message_chunk':
      case 'agent_thought_chunk':
        this.#chunk(chunkTypes[update.sessionUpdate], update);
        break;
      case 'tool_call':
        this.#toolCall(update);
        break;
      case 'tool_call_update':
        this.#toolCallUpdate(update);
        break;
      case 'plan':
        this.#planUpdate(update.entries);
        break;
      case 'compaction_update':
        this.#compaction(update.compactionId, (item) => {
          item.status = update.status;
          if (update.summary) item.summary = update.summary.map(blockText).join('');
          if (update.error) item.error = update.error;
        });
        break;
      case 'compaction_summary_chunk':
        this.#compaction(update.compactionId, (item) => {
          item.summary += blockText(update.content);
        });
        break;
      case 'notice':
        this.#push({
          type: 'notice',
          key: this.#key(),
          severity: update.severity,
          title: update.title,
          description: update.description,
        });
        break;
    }
  }

  #key() {
    return `i${this.#seq++}`;
  }

  #push<T extends TranscriptItem>(item: T): T {
    this.items.push(item);
    // Return the reactive proxy so later mutations are tracked.
    return this.items[this.items.length - 1] as T;
  }

  #chunk(type: MessageItem['type'], chunk: ContentChunk) {
    const part = blockToPart(chunk.content);
    if (!part) return;
    const messageId = chunk.messageId ?? null;
    const last = this.items.at(-1);

    // Without message IDs, consecutive chunks of the same kind are one message.
    if (last?.type === type && last.messageId === messageId) {
      const lastPart = last.parts.at(-1);
      if (part.type === 'text' && lastPart?.type === 'text') {
        lastPart.text += part.text;
      } else {
        last.parts.push(part);
      }
      return;
    }

    if (type === 'user') {
      this.#plan = null;
    }
    const sloppodMeta = chunk._meta?.sloppod as { steering?: boolean; notice?: boolean } | undefined;
    this.#push({
      type,
      key: this.#key(),
      messageId,
      steering: sloppodMeta?.steering === true,
      notice: sloppodMeta?.notice === true,
      parts: [part],
    });
  }

  #toolCall(call: ToolCall) {
    const existing = this.#tools.get(call.toolCallId);
    if (existing) {
      this.#toolCallUpdate(call);
      return;
    }
    const item = this.#push<ToolItem>({
      type: 'tool',
      key: this.#key(),
      toolCallId: call.toolCallId,
      title: call.title,
      kind: call.kind ?? 'other',
      status: call.status ?? 'pending',
      content: call.content ?? [],
      locations: call.locations ?? [],
      rawInput: call.rawInput,
      terminals: {},
    });
    this.#tools.set(call.toolCallId, item);
    this.#terminalMeta(item, call._meta);
  }

  #toolCallUpdate(update: ToolCallUpdate) {
    let item = this.#tools.get(update.toolCallId);
    if (!item) {
      this.#toolCall({ ...update, title: update.title ?? 'Tool call' } as ToolCall);
      return;
    }
    if (update.title) item.title = update.title;
    if (update.kind) item.kind = update.kind;
    if (update.status) item.status = update.status;
    if (update.content) item.content = update.content;
    if (update.locations) item.locations = update.locations;
    if (update.rawInput !== undefined) item.rawInput = update.rawInput;
    this.#terminalMeta(item, update._meta);
  }

  #terminalMeta(item: ToolItem, meta: Record<string, unknown> | null | undefined) {
    for (const key of ['terminal_output', 'terminal_output_delta']) {
      const output = meta?.[key] as { terminal_id?: string; data?: string } | undefined;
      if (output?.terminal_id && typeof output.data === 'string') {
        item.terminals[output.terminal_id] = (item.terminals[output.terminal_id] ?? '') + output.data;
      }
    }
  }

  /** A plan update replaces the plan of the current turn; a new turn gets a new plan. */
  #planUpdate(entries: PlanEntry[]) {
    if (this.#plan) {
      this.#plan.entries = entries;
    } else {
      this.#plan = this.#push<PlanItem>({ type: 'plan', key: this.#key(), entries });
    }
  }

  #compaction(compactionId: string, mutate: (item: CompactionItem) => void) {
    let item = this.#compactions.get(compactionId);
    if (!item) {
      item = this.#push<CompactionItem>({
        type: 'compaction',
        key: this.#key(),
        compactionId,
        status: 'in_progress',
        summary: '',
      });
      this.#compactions.set(compactionId, item);
    }
    mutate(item);
  }

  /** The tool call a permission request refers to, if the transcript has seen it. */
  tool(toolCallId: string): ToolItem | undefined {
    return this.#tools.get(toolCallId);
  }
}
