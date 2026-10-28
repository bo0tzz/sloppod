# Runtime API

The runtime serves everything on one HTTP port (default 8080): the REST API under `/api/v1`, ACP over WebSocket at `/acp`, and the UI's static files at `/`.

It has no authentication of its own; deploy it behind an authenticating proxy that sets the user's name in a header (default `X-Forwarded-User`). Every request acts as that user, and users only see their own sessions, proposals and credentials. Without the header, requests act as the configured default user, or are refused if there is none.

## REST

All bodies are JSON. Errors are `{"error": "<message>"}` with a 4xx/5xx status.

### Sessions

```
GET    /api/v1/sessions             -> Session[]   (most recently active first)
POST   /api/v1/sessions             -> Session     body: NewSession
GET    /api/v1/sessions/{id}        -> Session
DELETE /api/v1/sessions/{id}        -> 204         parks the session (cancelling its turn) and marks it deleted; restorable for 7 days, then purged with its pod, volume and keys
GET    /api/v1/sessions/deleted     -> Session[]   deleted sessions not purged yet, most recently deleted first
POST   /api/v1/sessions/{id}/restore -> Session    brings a deleted session back, parked
POST   /api/v1/sessions/{id}/park   -> Session     ends its pod and keeps its volume, transcript, branch, grants and keys; 409 during a turn
POST   /api/v1/sessions/{id}/wake   -> Session     brings a parked session back (no-op otherwise); a message sent to it does too
POST   /api/v1/sessions/{id}/repos  -> Session     body: {repo: string}; grants write access to another repository and tells the agent (no-op if it has it)
PUT    /api/v1/sessions/{id}/review -> Session     body: {review: "off" | "required"}
GET    /api/v1/repos                -> string[]    repositories sessions can be granted, e.g. ["bo0tzz/sloppod"]
GET    /api/v1/me                   -> {user: string}
GET    /api/v1/settings             -> Settings
PUT    /api/v1/settings             -> Settings    body: {model?: string | null, review?: "off" | "required"}; fields left out stay as they are
```

```ts
// The user's defaults for new sessions. `models` is what the agent offered in the user's most recent session
// (empty before the first one); `model` null means the agent's default. `review` is new sessions' review policy.
type Settings = {
  model: string | null;
  models: { value: string; name: string }[];
  review: "off" | "required";
};
```

A session starts from its first message: the UI opens an empty chat, and sending the first message creates the session. The runtime derives the session's repository from that message: an `owner/name` the user's GitHub App can access, or a bare repository name that matches exactly one of them. Without one, the session has no repository ([ADR 1](adr/0001-sessions-decoupled-from-directories.md)). Messages sent before the agent is up are queued and shown immediately.

```ts
type NewSession = {
  prompt: string;        // the first message; recorded right away and sent once the agent is up
  repo?: string;         // "owner/name"; derived from the prompt when absent
  title?: string;        // kept as given; without one, the agent's automatic title is used
  agent?: string;        // the agent to run: "claude-code" (the default) or "codex"
};

type Session = {
  id: string;            // UUID; also the ACP sessionId
  title: string;
  repo: string | null;   // from the session's repo grant, if any
  branch: string | null; // the session branch, e.g. "sloppod/3f2a…"
  grants: Grant[];
  phase: Phase;
  agent: string;         // "claude-code" or "codex"
  kind: "work" | "review"; // "review": a session reviewing a proposal (ADR 24); `GET /sessions` leaves these out
  review: "off" | "required"; // whether its pull requests are reviewed by another model first; from the user's settings
  created_at: string;    // RFC 3339
  updated_at: string;
  active_at: string;   // when something last happened (an event was recorded); `GET /sessions` is ordered by it, newest first
  deleted_at?: string;   // when it was deleted; `GET /sessions` leaves deleted sessions out
};

// What the session may do (ADR 2). Capabilities so far: "model", "github" (read: public data, plus the "repo"
// grants), "packages" (package registries), "codex" (Codex through the owner's ChatGPT login), "repo"
// {repo, access: "rw", branch}, "memory" {namespace, access: "rw"}. `description` says what the grant allows in plain
// terms, in the same words the agent's `list_grants` tool uses.
type Grant = { capability: string; params: Record<string, unknown>; description?: string };

type Phase =
  | "starting"           // pod being created or agent not connected yet
  | "idle"               // agent connected, no turn running
  | "working"            // a turn is running
  | "waiting"            // a turn is blocked on a permission request or a question
  | "disconnected"       // pod went away; the runtime is bringing it back
  | "parked"             // no pod (ADR 5); messages queue, and sending one wakes it
  | "stopped";
```

`POST /api/v1/sessions` derives the repository and grants before it returns. A session gains more repositories later through `POST /api/v1/sessions/{id}/repos`, or when the agent requests one and the user approves (`sloppod.repo.grant` proposals). `repo` is the repository the session was started for, which is checked out as its working directory, or else the first one it was given; `branch` is its session branch, the same in every repository. Until the agent names the session, its title is the first line of the first message. It returns `409` when the user has no credential for the agent's model (a Claude account for Claude Code, a ChatGPT login for Codex), and `400` with `{error}` when a given `repo` isn't accessible or `agent` isn't one the runtime has.

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
  | "memini"             // secret: a memini admin key; config: {url: string, home?: string} (home: your personal namespace)
  | "chatgpt";           // secret: the auth.json of a `codex login` only sloppod uses; config: {account_id} (derived)

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

Outward actions and access the agent asked for ([ADR 3](adr/0003-proposals-and-trusted-executor.md), [ADR 2](adr/0002-capabilities.md)). Kinds:

- `github.pr.create`: payload `{repo, base, head, head_sha, title, body, draft, auto_merge}`, proposed with an optional `review` (below); the detail adds `diff` and `commits`, up to `head_sha`, the commit that was proposed. Approving opens the PR only if `head` still points there; otherwise it fails and the agent proposes again. With `auto_merge`, which the agent can ask for and the reviewer decides, sloppod turns on GitHub's auto-merge after opening the PR (with the first merge method the repository allows), so it merges once its required checks pass, and only at the reviewed commit (`expectedHeadOid`); the result's `message` says whether that worked. Pushing to a head with auto-merge on turns it off: the first push is refused with a message saying so, and the PR then needs merging by hand.
- `github.stack.create`: pull requests that build on each other, e.g. branches managed with `gh stack`; payload `{repo, base, pull_requests: {head, base, head_sha, title, body, draft}[], auto_merge}`, proposed with an optional `review` like a pull request; bottom first, each based on the one below and the first on `base`. The detail adds `layers: {diff, commits}[]`, one per pull request. Approving checks that every head is still at its `head_sha` before opening anything, opens them (reusing open ones), and links them as a stack on GitHub; the result is `{pull_requests: {url, number}[], stack, message}`, with `stack` null and the reason in `message` if linking failed. With `auto_merge`, which the agent can ask for and the reviewer decides, sloppod turns on auto-merge for the top pull request at its proposed commit once the stack is linked, which merges the whole stack when its required checks pass; without a linked stack it stays off. Edits: `{auto_merge?, pull_requests: {title?, body?}[]}`, the titles and bodies by position.
- `sloppod.repo.grant`: write access to a repository; payload `{repo, reason, branch}`; the detail adds nothing.
- `github.issue.create`: issues to file in a repository the session can push to; payload `{repo, issues: {title, body, labels}[]}`; the detail adds nothing. Labels that don't exist yet are created. The result lists the issues filed, and any that failed.

**Review first** ([ADR 24](adr/0024-automated-review-before-human-review.md)): when the session's `review` is `required`, or the agent proposes a pull request or a stack with `review: true`, and the user has a ChatGPT login, the proposal starts `in_review` rather than `pending`. Without the login, a required review is skipped, and asking for one is an error. The runtime starts a review session (`kind: "review"`, running Codex, with read-only access to the repository and the commit under review checked out, detached, each time its pod starts), which ends each round by calling its `submit_review` tool. A pass moves the proposal to `pending`, where the user sees it and standing approvals can match it. Changes requested move it to `changes_requested` and the findings go to the agent, not the user; the agent pushes fixes and calls `request_review` (with an `update` of the proposal's fields, e.g. a title and body that fit the change again, and replies to findings it disputes), which re-pins the proposal to its branch's new head and runs the next round in the same review session. After three rounds with changes requested, the proposal goes to `pending` anyway, with the open findings in its reviews. Between rounds the review session is parked (ADR 5), and the next round wakes it on the new commit. It's deleted once the proposal passes, reaches that cap or is withdrawn, and when the author's session is deleted its proposals still in review are withdrawn. A round that stalls doesn't keep the proposal in review: a reviewer that ends its turn without calling `submit_review` is reminded once, and if it does so again, or the round takes over 30 minutes, the proposal goes to `pending` unreviewed (the author is told, and no standing approval matches it). `GET /api/v1/proposals/{id}/reviews` lists the rounds.

When a proposal is decided or carried out, the runtime tells the agent: the notice is recorded as a `user_message_chunk` with `_meta.sloppod.notice: true` and delivered as a new turn when the agent is idle, steered into a running turn, or queued until the agent is up. Sessions whose turn a runtime restart cut off get a notice of the same kind once their agent is back, naming the build now running (`SLOPPOD_BUILD`, the chart's app version).

```
GET  /api/v1/proposals?session={id}&status={status}  -> Proposal[]   400 for a status that isn't one
GET  /api/v1/proposals/{id}                          -> ProposalDetail
GET  /api/v1/proposals/{id}/reviews                  -> Review[]   its review rounds, first to last
POST /api/v1/proposals/{id}/approve                  -> Proposal   body: {} or edits, per kind (below); 400 for edits that don't apply, 409 once decided
POST /api/v1/proposals/{id}/reject                   -> Proposal   body: {reason?: string}; 409 once decided
```

```ts
type Proposal = {
  id: string;
  session_id: string;
  kind: "github.pr.create" | "github.stack.create" | "github.issue.create" | "sloppod.repo.grant";
  status: "in_review" | "changes_requested" | "pending" | "approved" | "rejected" | "executed" | "failed" | "withdrawn";
  payload: Record<string, unknown>;  // per kind, see above
  result?: Record<string, unknown>;            // when executed; pull requests: {url, number}; issues: {issues: {title, url, number}[], failed: {title, error}[], message}
  error?: string;                              // when failed
  reason?: string;                             // when rejected
  created_at: string;
  decided_at?: string;
  decided_by?: string;                         // the user, or "standing-approval:<id>"
  review_session_id?: string;                  // the session reviewing it, or that did
};

type Review = {
  round: number;          // 1, 2, 3
  head_sha: string;       // the commit reviewed
  replies?: string;       // the author's replies to the previous round's findings
  verdict?: "pass" | "changes";  // absent while the round runs
  summary?: string;
  // id: "<round>.<n>"; a finding a later round reports again keeps its id. severity: p0 must be fixed before
  // merging, p1 should be fixed, p2 is optional polish.
  findings: { id: string; severity: "p0" | "p1" | "p2"; category: "correctness" | "security" | "performance" | "reliability" | "maintainability" | "tests"; title: string; explanation: string; path?: string; line?: number; end_line?: number; suggested_fix?: string }[];
  resolved: string[];     // ids of the previous round's findings that no longer hold
  created_at: string;
  finished_at?: string;
};

type ProposalDetail = Proposal & {
  // github.pr.create:
  diff?: string;          // unified diff of base...head
  commits?: { sha: string; message: string }[];
  // github.stack.create: each pull request's, from the base up
  layers?: { diff: string; commits: { sha: string; message: string }[] }[];
};
```

Approving returns as soon as the decision is recorded, with status `approved`; the proposal is carried out in the background and its outcome (`executed` or `failed`) follows as a `_sloppod/proposal` update, and in `GET /api/v1/proposals/{id}`. A proposal still `approved` when the runtime restarts is carried out again on startup; executors don't repeat work already done (an open pull request from the same head into the same base is reused, and issues with the same title filed in the last day aren't filed again). Approving with edits changes the payload first: `github.pr.create` takes `{title?, body?, auto_merge?}`; `github.stack.create` takes `{pull_requests?: {title?, body?}[], auto_merge?}`, the pull requests' edits by position; `github.issue.create` takes `{issues}`, the issues to file, so leaving one out drops it. `sloppod.repo.grant` takes no edits. A proposal's status only moves forward (`in_review` and `changes_requested`, back and forth between review rounds; then `pending`; then `approved` or `rejected`; then `executed` or `failed`), so a UI can ignore a copy, e.g. from a slow refetch, that is behind the one it has. Undecided proposals, in review or not, can be withdrawn.

### Standing approvals

Pre-approved proposal patterns ([ADR 3](adr/0003-proposals-and-trusted-executor.md)): each standing approval covers one session, and a proposal from that session that matches it (its kind, and its target if it names one) is approved as soon as it's made, with the standing approval's edits, and carried out like any other approval. Its `decided_by` names the standing approval, and the agent's tool reply says it was approved. Every built-in kind's target is its repository.

```
GET    /api/v1/standing-approvals        -> StandingApproval[]   (live ones: not revoked, not expired)
POST   /api/v1/standing-approvals        -> StandingApproval     body: {kind, session_id, target?, edits?, expires_at?}; 400 without session_id
DELETE /api/v1/standing-approvals/{id}   -> 204                  revokes it
```

```ts
type StandingApproval = {
  id: string;
  kind: string;          // a proposal kind, e.g. "github.pr.create"
  target?: string;       // owner/name; absent matches any
  session_id: string;    // the one session it covers
  edits: { auto_merge?: boolean };  // the only edit a standing approval can make
  expires_at?: string;   // RFC 3339; absent means until revoked
  created_at: string;
};
```

## ACP at `/acp`

A WebSocket carrying [ACP](https://agentclientprotocol.com) JSON-RPC 2.0, one message per text frame. The runtime is the agent from the UI's point of view. ACP `sessionId`s are runtime session IDs. Sessions are created through REST, not `session/new`.

UI → runtime:

| Method | Notes |
|---|---|
| `initialize` | Returns the agent's capabilities, including `_meta.steering`. |
| `session/load` `{sessionId, cwd: "/", mcpServers: []}` | Replays the session's history as `session/update` notifications, then responds. From then on the connection receives that session's live updates. Several sessions can be loaded on one connection. |
| `session/prompt` `{sessionId, prompt: ContentBlock[]}` | Starts a turn. Responds `{stopReason}` when the turn ends. While the agent isn't up (`starting`, `disconnected`) the prompt is queued: it's echoed right away, everything queued runs as one turn once the agent is ready, and each queued request responds when that turn ends. While a turn is running, the prompt waits behind it instead: it isn't echoed yet, it shows in `_sloppod/queue`, and everything waiting runs as the next turn when this one ends (echoed then). Queued prompts and notices survive a runtime restart: they're kept until a turn takes them, then run once the agent is back (or wake a parked session); a prompt request pending across a restart gets no response, since its connection is gone, but its turn shows in the transcript. With `_meta.sloppod.messageId` (a UUID), the message is echoed, and queued, under that id, so a UI whose connection dropped before the response can tell from the next `session/load` whether the message arrived, and send it again if not. `_session/steering` takes the same. |
| `session/cancel` `{sessionId}` (notification) | Cancels the running turn. The runtime answers the turn's open permission requests and questions as cancelled (the client needn't, unlike plain ACP) and sends `_sloppod/permission_resolved` for them; `ask_user` questions stay open. Messages still queued for an agent that isn't up stay queued. |
| `_sloppod/steer_queued` `{sessionId, id}` | Moves a message waiting behind the running turn into it, as a steer (echoed with `_meta.sloppod.steering`). Its `session/prompt` request then responds. |
| `_session/steering` `{sessionId, prompt: ContentBlock[]}` | Delivers a message into the running turn. Before the agent is up it joins the queue like a prompt and returns `{outcome: "queued"}`. |
| `session/set_config_option` `{sessionId, configId, value}` | Changes one of the session's config options from the load response's `configOptions`, e.g. `model`. Responds `{configOptions}`. The session keeps its model across restarts. |

Sessions always run in their agent's most autonomous mode, Claude Code's auto mode or Codex's full access: the sandbox and the session's grants bound what any tool call can reach. Mode isn't configurable.

Runtime → UI:

| Method | Notes |
|---|---|
| `session/update` | Standard ACP updates: `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `config_option_update`, and compaction reports. Prompts sent by any UI are echoed as `user_message_chunk` with a `messageId` per prompt, so every viewer sees them; steers carry `_meta.sloppod.steering: true`, and notices to the agent (see Proposals) `_meta.sloppod.notice: true`. |
| `session/request_permission` (request) | Sent to every UI that has the session loaded, including requests still open when a UI loads the session (after the `session/load` response). The first answer wins. |
| `elicitation/create` (request) | The agent asks the user something, e.g. with `ask_user` (below). Delivered like a permission request. Params: `{sessionId, mode: "form", message, requestedSchema, toolCallId?}`; the schema is an object of primitive fields (below). Answer `{action: "accept", content}`, `{action: "decline"}` (skip: the agent is told the user didn't answer) or `{action: "cancel"}`. |
| `_sloppod/queue` `{sessionId, messages: {id, prompt}[]}` | The messages waiting behind the running turn, sent whenever that changes and when a UI loads the session. |
| `_sloppod/permission_resolved` `{sessionId, toolCallId, requestId}` | Sent to every UI, including the one that answered, once a permission request or question is answered or cancelled. `requestId` is the JSON-RPC id the request had on this connection. |
| `_sloppod/session` `{session: Session}` | The session's phase, title, repository or grants changed. |
| `_sloppod/proposal` `{proposal: Proposal}` | A proposal for a loaded session was created or changed. |

### Questions

Agents ask the user questions with sloppod's `ask_user` tool, which doesn't wait for the answer ([ADR 4](adr/0004-asynchronous-interaction.md)); Claude Code's own `AskUserQuestion`, which would stop the turn, is turned off. The runtime sends the question to UIs as an `elicitation/create` request like any other, but nothing waits on it: the session doesn't go `waiting`, a cancelled turn or an agent reconnect leaves it open, and the answer reaches the agent as a notice (`sloppod: the user answered your question: …`), or that it was dismissed; either names the questions. Either kind arrives as one form with two fields per question, in order: `question_<n>` and `question_<n>_custom`.

- `question_<n>` is `{type: "string", oneOf: [{const, title, description?}]}` for a single choice, or `{type: "array", items: {anyOf: [{const, title, description?}]}}` for several. `title` is the question's short header; `description` holds the question text when there are several questions (with one, it's the `message`). An option's `_meta["_claude/askUserQuestionOption"].preview` may carry a preview (Markdown/code).
- `question_<n>_custom` is a free-text "Other" answer: for a single choice it's the answer when nothing is picked, or a note on the pick; for several it's added to them.
- Nothing is required. `content` maps field keys to the picked `const` (or array of them) and the typed text.

Other form elicitations (from MCP servers) use the same primitive field types: `string` (optionally with `oneOf`/`enum`), `number`, `integer`, `boolean`, and arrays of enums.

## Session pods

Session pods use three more endpoints, on their own port (8081, `sessions`); that port is the only one they can reach, so they can't use the API above. Keep it off public routes.

- `/link/v1`: the supervisor link ([ADR 17](adr/0017-in-pod-supervisor.md)), a WebSocket authenticated with the pod's projected ServiceAccount token. Its types are in `crates/protocol`.
- `/mcp`: an MCP server (streamable HTTP, JSON responses) registered with the agent at session start. It identifies the session by the pod's IP. Its tools:
  - `propose_pull_request {title, body, repo?, base?, head?, draft?, auto_merge?}` creates a `github.pr.create` proposal from the session branch, or from `head`, a branch named after it (`<session branch>-<name>`, the name of letters, digits, `.`, `_` and `-`), so a session can have several open; `repo` is needed when the session can write to more than one.
  - `propose_issues {issues: {title, body?, labels?}[], repo?}` creates a `github.issue.create` proposal (up to 50 issues; `repo` as for pull requests).
  - `request_repository {repo, reason}` creates a `sloppod.repo.grant` proposal.
  - `get_proposal {id}` returns a proposal's status and result. The agent is also told when its proposals are decided.
  - `withdraw_proposal {id, reason?}` withdraws one of the session's pending proposals: it becomes `withdrawn` (with the reason) and nothing is carried out.
- `/memini/*`: memini, for sessions with a `memory` grant, identified by the pod's IP. The runtime forwards each request to the owner's memini with the session's own key and namespace, and refuses memini's administration endpoints. The memini plugin's `MEMINI_BASE_URL` points here.

In the pod, the repository is checked out at `/workspace/repo` on the session branch. `/workspace` is the session's own volume, kept across pod restarts.
