# Runtime API

The runtime serves everything on one HTTP port (default 8080): the REST API under `/api/v1`, ACP over WebSocket at `/acp`, and the UI's static files at `/`.

It has no authentication of its own; deploy it behind an authenticating proxy that sets the user's name in a header (default `X-Forwarded-User`). Every request acts as that user, and users only see their own sessions, proposals and credentials. Without the header, requests act as the configured default user, or are refused if there is none.

## REST

All bodies are JSON. Errors are `{"error": "<message>"}` with a 4xx/5xx status.

### Sessions

```
GET    /api/v1/sessions             -> Session[]   (most recently updated first)
POST   /api/v1/sessions             -> Session     body: NewSession
GET    /api/v1/sessions/{id}        -> Session
DELETE /api/v1/sessions/{id}        -> 204         stops the session and deletes its pod and volume
GET    /api/v1/repos                -> string[]    repositories sessions can be granted, e.g. ["bo0tzz/sloppod"]
GET    /api/v1/me                   -> {user: string}
```

A session starts from its first message: the UI opens an empty chat, and sending the first message creates the session. The runtime derives the session's repository from that message: an `owner/name` the user's GitHub App can access, or a bare repository name that matches exactly one of them. Without one, the session has no repository ([ADR 1](adr/0001-sessions-decoupled-from-directories.md)). Messages sent before the agent is up are queued and shown immediately.

```ts
type NewSession = {
  prompt: string;        // the first message; recorded right away and sent once the agent is up
  repo?: string;         // "owner/name"; derived from the prompt when absent
  title?: string;        // kept as given; without one, the agent's automatic title is used
};

type Session = {
  id: string;            // UUID; also the ACP sessionId
  title: string;
  repo: string | null;   // from the session's repo grant, if any
  branch: string | null; // the session branch, e.g. "sloppod/3f2a…"
  grants: Grant[];
  phase: Phase;
  created_at: string;    // RFC 3339
  updated_at: string;
};

// What the session may do (ADR 2). Capabilities so far: "model", "github" (read), "packages" (package
// registries), "repo" {repo, access: "rw", branch}, "memory" {namespace, access: "rw"}.
type Grant = { capability: string; params: Record<string, unknown> };

type Phase =
  | "starting"           // pod being created or agent not connected yet
  | "idle"               // agent connected, no turn running
  | "working"            // a turn is running
  | "waiting"            // a turn is blocked on a permission request
  | "disconnected"       // pod went away; the runtime is bringing it back
  | "stopped";
```

`POST /api/v1/sessions` derives the repository and grants before it returns; they don't change afterwards. Until the agent names the session, its title is the first line of the first message. It returns `409` when the user has no Claude account configured, and `400` with `{error}` when a given `repo` isn't accessible.

### Credentials

Accounts and keys the runtime uses on a user's behalf ([ADR 21](adr/0021-credentials-live-in-the-runtime.md)). Secrets are write-only: the API never returns them. The first credential of a kind becomes the default for it; deleting the default promotes the oldest remaining one. Running sessions use whichever is the default when they make a request. An invalid credential (unknown kind, a private key that doesn't parse, a URL without a host) gets `400` with `{error}`.

```
GET    /api/v1/credentials                -> Credential[]
POST   /api/v1/credentials                -> Credential   body: NewCredential
POST   /api/v1/credentials/{id}/default   -> Credential   makes it the default for its kind
DELETE /api/v1/credentials/{id}           -> 204
```

```ts
type CredentialKind =
  | "claude"             // secret: a token from `claude setup-token`; config: {}
  | "github-app"         // secret: the app's private key (PEM); config: {app_id: string}
  | "memini";            // secret: a memini admin key; config: {url: string, home?: string} (home: your personal namespace)

type NewCredential = { kind: CredentialKind; name: string; config: Record<string, string>; secret: string };

type Credential = {
  id: string;
  kind: CredentialKind;
  name: string;
  config: Record<string, string>;
  default: boolean;
  created_at: string;
};
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
| `session/prompt` `{sessionId, prompt: ContentBlock[]}` | Starts a turn. Responds `{stopReason}` when the turn ends. While the agent isn't up (`starting`, `disconnected`) the prompt is queued: it's echoed right away, everything queued runs as one turn once the agent is ready, and each queued request responds when that turn ends. |
| `session/cancel` `{sessionId}` (notification) | Cancels the running turn. Messages still queued for an agent that isn't up stay queued. |
| `_session/steering` `{sessionId, prompt: ContentBlock[]}` | Delivers a message into the running turn. Before the agent is up it joins the queue like a prompt and returns `{outcome: "queued"}`. |
| `session/set_mode` `{sessionId, modeId}` | Switches the permission mode. |

Runtime → UI:

| Method | Notes |
|---|---|
| `session/update` | Standard ACP updates: `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `current_mode_update`, and compaction reports. Prompts sent by any UI are echoed as `user_message_chunk` with a `messageId` per prompt, so every viewer sees them; steers carry `_meta.sloppod.steering: true`. |
| `session/request_permission` (request) | Sent to every UI that has the session loaded, including requests still open when a UI loads the session (after the `session/load` response). The first answer wins. |
| `_sloppod/permission_resolved` `{sessionId, toolCallId}` | Sent to every UI, including the one that answered, once a request is answered or cancelled. |
| `_sloppod/session` `{session: Session}` | The session's phase or title changed. |
| `_sloppod/proposal` `{proposal: Proposal}` | A proposal for a loaded session was created or changed. |

## Session pods

Session pods use two more endpoints, on their own port (8081, `sessions`); that port is the only one they can reach, so they can't use the API above. Keep it off public routes.

- `/link/v1`: the supervisor link ([ADR 17](adr/0017-in-pod-supervisor.md)), a WebSocket authenticated with the pod's projected ServiceAccount token. Its types are in `crates/protocol`.
- `/mcp`: an MCP server (streamable HTTP, JSON responses) registered with the agent at session start. It identifies the session by the pod's IP. Its tools:
  - `propose_pull_request {title, body, base?, draft?}` creates a `github.pr.create` proposal from the session branch.
  - `get_proposal {id}` returns a proposal's status and result. The agent isn't notified when a proposal is decided; it checks with this tool.

In the pod, the repository is checked out at `/workspace/repo` on the session branch. `/workspace` is the session's own volume, kept across pod restarts.
