# Runtime API

The runtime serves everything on one HTTP port (default 8080): the REST API under `/api/v1`, ACP over WebSocket at `/acp`, and the UI's static files at `/`. It has no authentication of its own; deploy it behind an authenticating proxy.

## REST

All bodies are JSON. Errors are `{"error": "<message>"}` with a 4xx/5xx status.

### Sessions

```
GET    /api/v1/sessions             -> Session[]   (most recently updated first)
POST   /api/v1/sessions             -> Session     body: NewSession
GET    /api/v1/sessions/{id}        -> Session
DELETE /api/v1/sessions/{id}        -> 204         stops the session and deletes its pod and volume
GET    /api/v1/repos                -> string[]    repositories sessions can be granted, e.g. ["bo0tzz/sloppod"]
```

```ts
type NewSession = {
  repo: string;          // "owner/name"
  title?: string;        // kept as given; without one, the agent's automatic title is used
  prompt?: string;       // sent as the first prompt once the agent is up
};

type Session = {
  id: string;            // UUID; also the ACP sessionId
  title: string;
  repo: string;
  branch: string;        // the session branch, e.g. "sloppod/3f2a…"
  phase: Phase;
  created_at: string;    // RFC 3339
  updated_at: string;
};

type Phase =
  | "starting"           // pod being created or agent not connected yet
  | "idle"               // agent connected, no turn running
  | "working"            // a turn is running
  | "waiting"            // a turn is blocked on a permission request
  | "disconnected"       // pod went away; the runtime is bringing it back
  | "stopped";
```

### Proposals

Outward actions the agent proposed ([ADR 3](adr/0003-proposals-and-trusted-executor.md)). The only kind so far is `github.pr.create`.

```
GET  /api/v1/proposals?session={id}&status={status}  -> Proposal[]
GET  /api/v1/proposals/{id}                          -> ProposalDetail
POST /api/v1/proposals/{id}/approve                  -> Proposal   body: {} (optional: {title?, body?} to edit before executing)
POST /api/v1/proposals/{id}/reject                   -> Proposal   body: {reason?: string}
```

```ts
type Proposal = {
  id: string;
  session_id: string;
  kind: "github.pr.create";
  status: "pending" | "approved" | "rejected" | "executed" | "failed";
  payload: { repo: string; base: string; head: string; title: string; body: string; draft: boolean };
  result?: { url: string; number: number };   // when executed
  error?: string;                              // when failed
  reason?: string;                             // when rejected
  created_at: string;
  decided_at?: string;
};

type ProposalDetail = Proposal & {
  diff: string;          // unified diff of base...head
  commits: { sha: string; message: string }[];
};
```

Approving executes the proposal immediately; the response carries the final status.

## ACP at `/acp`

A WebSocket carrying [ACP](https://agentclientprotocol.com) JSON-RPC 2.0, one message per text frame. The runtime is the agent from the UI's point of view. ACP `sessionId`s are runtime session IDs. Sessions are created through REST, not `session/new`.

UI → runtime:

| Method | Notes |
|---|---|
| `initialize` | Returns the agent's capabilities, including `_meta.steering`. |
| `session/load` `{sessionId, cwd: "/", mcpServers: []}` | Replays the session's history as `session/update` notifications, then responds. From then on the connection receives that session's live updates. Several sessions can be loaded on one connection. |
| `session/prompt` `{sessionId, prompt: ContentBlock[]}` | Starts a turn. Responds `{stopReason}` when the turn ends. |
| `session/cancel` `{sessionId}` (notification) | Cancels the running turn. |
| `_session/steering` `{sessionId, prompt: ContentBlock[]}` | Delivers a message into the running turn. |
| `session/set_mode` `{sessionId, modeId}` | Switches the permission mode. |

Runtime → UI:

| Method | Notes |
|---|---|
| `session/update` | Standard ACP updates: `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `current_mode_update`, and compaction reports. Prompts sent by any UI are echoed as `user_message_chunk` with a `messageId` per prompt, so every viewer sees them; steers carry `_meta.sloppod.steering: true`. |
| `session/request_permission` (request) | Sent to every UI that has the session loaded, including requests still open when a UI loads the session (after the `session/load` response). The first answer wins. |
| `_sloppod/permission_resolved` `{sessionId, toolCallId}` | Sent to every UI, including the one that answered, once a request is answered or cancelled. |
| `_sloppod/session` `{session: Session}` | The session's phase or title changed. |
| `_sloppod/proposal` `{proposal: Proposal}` | A proposal for a loaded session was created or changed. |
