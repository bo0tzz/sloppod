// The subset of the ACP v1 schema the UI reads. Field names follow the schema.

export type Meta = { _meta?: Record<string, unknown> | null };

export type ContentBlock =
  | ({ type: 'text'; text: string } & Meta)
  | ({ type: 'image'; data: string; mimeType: string; uri?: string | null } & Meta)
  | ({ type: 'audio'; data: string; mimeType: string } & Meta)
  | ({ type: 'resource_link'; uri: string; name: string; title?: string | null } & Meta)
  | ({ type: 'resource'; resource: { uri: string; text?: string; blob?: string; mimeType?: string | null } } & Meta);

export type ContentChunk = { content: ContentBlock; messageId?: string | null } & Meta;

export type ToolKind = 'read' | 'edit' | 'delete' | 'move' | 'search' | 'execute' | 'think' | 'fetch' | 'switch_mode' | 'other';
export type ToolCallStatus = 'pending' | 'in_progress' | 'completed' | 'failed';

export type ToolCallContent =
  | ({ type: 'content'; content: ContentBlock } & Meta)
  | ({ type: 'diff'; path: string; oldText?: string | null; newText: string } & Meta)
  | ({ type: 'terminal'; terminalId: string } & Meta);

export type ToolCallLocation = { path: string; line?: number | null } & Meta;

export type ToolCall = {
  toolCallId: string;
  title: string;
  kind?: ToolKind;
  status?: ToolCallStatus;
  content?: ToolCallContent[];
  locations?: ToolCallLocation[];
  rawInput?: unknown;
  rawOutput?: unknown;
} & Meta;

export type ToolCallUpdate = {
  toolCallId: string;
  title?: string | null;
  kind?: ToolKind | null;
  status?: ToolCallStatus | null;
  content?: ToolCallContent[] | null;
  locations?: ToolCallLocation[] | null;
  rawInput?: unknown;
  rawOutput?: unknown;
} & Meta;

export type PlanEntry = {
  content: string;
  priority: 'high' | 'medium' | 'low';
  status: 'pending' | 'in_progress' | 'completed';
};

export type SessionMode = { id: string; name: string; description?: string | null };
export type SessionModeState = { currentModeId: string; availableModes: SessionMode[] };

export type AvailableCommand = { name: string; description: string };

export type SessionUpdate =
  | ({ sessionUpdate: 'user_message_chunk' } & ContentChunk)
  | ({ sessionUpdate: 'agent_message_chunk' } & ContentChunk)
  | ({ sessionUpdate: 'agent_thought_chunk' } & ContentChunk)
  | ({ sessionUpdate: 'tool_call' } & ToolCall)
  | ({ sessionUpdate: 'tool_call_update' } & ToolCallUpdate)
  | ({ sessionUpdate: 'plan'; entries: PlanEntry[] } & Meta)
  | ({ sessionUpdate: 'available_commands_update'; availableCommands: AvailableCommand[] } & Meta)
  | ({ sessionUpdate: 'current_mode_update'; currentModeId: string } & Meta)
  | ({ sessionUpdate: 'session_info_update'; title?: string | null; updatedAt?: string | null } & Meta)
  | ({ sessionUpdate: 'notice'; severity: string; title: string; description?: string | null } & Meta)
  | ({
      sessionUpdate: 'compaction_update';
      compactionId: string;
      status: string;
      summary?: ContentBlock[] | null;
      error?: string | null;
    } & Meta)
  | ({ sessionUpdate: 'compaction_summary_chunk'; compactionId: string; content: ContentBlock } & Meta)
  | ({ sessionUpdate: 'usage_update' | 'config_option_update' } & Meta);

export type SessionNotification = { sessionId: string; update: SessionUpdate };

export type PermissionOption = {
  optionId: string;
  name: string;
  kind: 'allow_once' | 'allow_always' | 'reject_once' | 'reject_always';
};

export type RequestPermissionRequest = {
  sessionId: string;
  toolCall: ToolCallUpdate;
  options: PermissionOption[];
};

export type InitializeResponse = {
  protocolVersion: number;
  agentCapabilities?: Record<string, unknown>;
  agentInfo?: { name: string; version?: string } | null;
  _meta?: { steering?: { supported?: boolean } } & Record<string, unknown>;
};

export type LoadSessionResponse = {
  modes?: SessionModeState | null;
} & Meta;

export type StopReason = 'end_turn' | 'max_tokens' | 'max_turn_requests' | 'refusal' | 'cancelled';
