# Runtime API

The runtime serves everything on one HTTP port (default 8080): the REST API under `/api/v1`, ACP over WebSocket at `/acp`, and the UI's static files at `/`.

It has no authentication of its own; deploy it behind an authenticating proxy that sets the user's name in a header (default `X-Forwarded-User`). Every request acts as that user, and users only see their own sessions, proposals and credentials. Without the header, requests act as the configured default user, or are refused if there is none.

## REST

All bodies are JSON. Errors are `{"error": "<message>"}` with a 4xx/5xx status.

### Sessions

```
GET    /api/v1/sessions             -> Session[]   (most recently active first)
POST   /api/v1/sessions             -> Session     body: NewSession
POST   /api/v1/sessions/suggest     -> StartingSet body: {prompt}; the starting set suggested for a first message as it's written (#232)
GET    /api/v1/sessions/{id}        -> Session
DELETE /api/v1/sessions/{id}        -> 204         parks the session (cancelling its turn) and marks it deleted, and its sub-sessions at any depth with it, including any restored below one already deleted; restorable for 7 days, then purged with its pod, volume and keys
GET    /api/v1/sessions/deleted     -> Session[]   deleted sessions not purged yet, most recently deleted first
POST   /api/v1/sessions/{id}/restore -> Session    brings a deleted session back, parked
POST   /api/v1/sessions/{id}/park   -> Session     ends its pod and keeps its volume, transcript, branch, grants and keys; 409 during a turn
POST   /api/v1/sessions/{id}/wake   -> Session     brings a parked session back (no-op otherwise); a message sent to it does too
POST   /api/v1/sessions/{id}/repos  -> Session     body: {repo: string, credential?: string}; grants write access to another repository and tells the agent (no-op if it has it); with `credential`, one of the user's `github-user` tokens, the session acts on it with that token (see below)
POST   /api/v1/sessions/{id}/grants -> Session (201) body: {capability, expires_at?, params?}; grants model, github, packages, docs or codex, `work` with `params: {graph, level?}` (one of the user's work graphs, at `read`, `track` (the default) or `plan`; one grant per graph), or one of the user's own capabilities by name (below); 409 if the session has it
PATCH  /api/v1/sessions/{id}/grants/{grant} -> Session  body: {expires_at?: string | null, narrow?: true, credentials?: string[]}; changes when it runs out (null: never), narrows it (a repository it can push to becomes read only), or, for a `model` or `codex` grant, sets the accounts it acts with, in order (below)
DELETE /api/v1/sessions/{id}/grants/{grant} -> Session  revokes it
PUT    /api/v1/sessions/{id}/review -> Session     body: {review: "off" | "required"}
PUT    /api/v1/sessions/{id}/volume -> Session     body: {kept: boolean}; keeps a stopped session's volume, or lets it be removed a week after the session stopped (#455)
GET    /api/v1/storage          -> {volumes: {name: string, capacity: number | null, session?: Session}[]}  every session volume the cluster holds (through the placement backend) that the user should see: their sessions', of any kind and deleted ones included, with the session (its `disk`, absent when never measured), and those of sessions the database doesn't know, without one; not other users'. `capacity` is the volume's size now in bytes: what it's been asked to grow to while an expansion is under way, which the cluster catches up with (#479)
PUT    /api/v1/sessions/{id}/read   -> {seq: number}  body: {seq: number}; the user has read the session up to the event `seq` (below); only moves forward, so a late report from another UI changes nothing, and answers the position as it stands; 400 for a negative one
GET    /api/v1/sessions/{id}/debug-bundle?message={messageId} -> DebugBundle  downloads (`Content-Disposition: attachment`) a debug bundle of the turn the user message `messageId` started; with `before={messageId}` instead, of the turn before that one; with neither, of the latest turn; 404 when the session has no such turn (below)
GET    /api/v1/sessions/{id}/work-done -> WorkDone[]  the issues and pull requests the session has acted on (#205), newest first, built from its carried-out proposals and their results (nothing of it is stored): one entry per item, with what was done to it (below)
GET    /api/v1/sessions/{id}/highlights -> Highlight[]  what the agent highlighted for the user in this session (#439), oldest first, the ones marked seen included
POST   /api/v1/sessions/{id}/highlights/{highlight}/seen -> Highlight  the user marks it seen; that's all it does: no turn starts and the agent isn't told. Marking one seen again changes nothing
POST   /api/v1/sessions/{id}/highlights/{highlight}/snooze -> Highlight  body: {until: string}; the user puts it off until then (#467), an RFC 3339 time: it stops counting in `waiting.highlights` until then, and the runtime brings it back at that time itself (below). 400 for a time that isn't one, 404 when it isn't the session's open highlight, or the agent has taken it back
GET    /api/v1/repos                -> string[]    repositories sessions can be granted, e.g. ["bo0tzz/sloppod"]
GET    /api/v1/me                   -> {user: string, version: string | null, build: string | null}   version: the release, e.g. 0.1.2-main.153 (SLOPPOD_VERSION, the chart's version); build: the commit or tag it was built from (SLOPPOD_BUILD, the chart's app version)
GET    /api/v1/settings             -> Settings
PUT    /api/v1/settings             -> Settings    body: {model?: string | null, sub_session_model?: string | null, review?: "off" | "required", timezone?: string, instructions?: string | null, session_budget?: number | null}; fields left out stay as they are; 400 for a timezone that isn't one, instructions over 32 KiB, or a session_budget that isn't a number of dollars above 0
```

```ts
// The user's defaults for new sessions. `models` is what the agent offered in the user's most recent session
// on its own models (empty before the first one; a session on an endpoint's model doesn't count, since Claude Code lists its aliases there with the endpoint's model behind each), then the models of the user's endpoints (below); `agent_models` is each agent's own, from the user's latest session of that agent on its own models, keyed by agent. `model` null means the agent's default. `review` is new sessions' review policy.
// `timezone` is the user's, as an IANA name (e.g. "Europe/Amsterdam"), null until set; the UI sets the browser's
// for a user without one. The time line agents get after a gap is in it, and their agents run with it as `TZ` from
// their next start (#253); without one, both are UTC.
// `sub_session_model` is what sub-sessions run on unless `spawn_session` names a model (#283): a model, "parent" for
// their parent's, or null (the default) for the agent's cheaper model where it has one, Sonnet for Claude Code, and their
// parent's otherwise.
// `instructions` are the user's own for all their sessions, like a user-level CLAUDE.md (#342), null until set, at most
// 32 KiB, trimmed. Work sessions and their sub-sessions get their owner's after the deployment's (the chart's
// `instructions`), from their agent's next start: in Claude Code's user-level CLAUDE.md, and in Codex's AGENTS.md ahead
// of the session prompt. Review sessions keep the reviewer's own.
type Settings = {
  model: string | null;
  sub_session_model: string | null;
  session_budget: number | null;  // the most a session on a LiteLLM operator model may spend, in dollars: its virtual key's maxBudget (#461); null for none
  models: { value: string; name: string }[];
  agent_models: { [agent: string]: { value: string; name: string }[] };
  review: "off" | "required";
  timezone: string | null;
  instructions: string | null;
};
```

**Debug bundles** ([#378](https://github.com/bo0tzz/sloppod/issues/378)) are for reporting a response the model provider stopped, e.g. with a safety classifier, to the provider. A bundle is one JSON file the user downloads, reads, and sends on themselves; sloppod never sends it anywhere. It covers the turn the user message `messageId` started (a turn runs from a message that isn't steered into a running turn to the next one; notices start turns too) and up to three turns before it. The chat offers it on each of the agent's messages, for the turn that message is in; where the loaded history begins part way through a turn, the messages before the first user message loaded ask with `before` (or for the latest turn, when no user message is loaded). Redaction covers object keys as well as strings. Credentials never reach a session's pod (ADR 11); the placeholders that stand in for them, and anything shaped like an Anthropic or GitHub token, are redacted anyway.

```ts
type DebugBundle = {
  format: "sloppod-debug-bundle/1";
  generated_at: string;
  note: string;
  runtime: { version: string | null; build: string | null };
  session: { id: string; agent: string; agent_session_id: string | null; model: string | null; created_at: string };
  // The ACP adapter's `agentInfo` from `initialize` (e.g. {name: "@agentclientprotocol/claude-agent-acp", version}),
  // kept with the session since #378; null for an agent that last started before then.
  adapter: { name?: string; title?: string; version?: string } | null;
  // From the agent's transcript in the turns covered: Claude Code's versions and the models it called.
  agent_versions: string[];
  models: string[];
  turn: string; // the chosen turn's messageId
  turns: {
    message_id: string;
    started_at: string;
    ended_at: string | null; // when the next turn started
    // Each model call in the turn, from the `requestId` Claude Code keeps on the assistant lines of its transcript
    // (ADR 1), matched to the turn by time; `stop_reason` is how the response ended (e.g. "refusal").
    model_calls: { request_id: string; timestamp: string; model: string | null; message_id: string | null;
                   stop_reason: string | null; file: string }[];
    events: { seq: number; at: string; update: SessionUpdate }[]; // the turn as the chat shows it
  }[];
  // The agent's own transcript (Claude Code's `<agent_session_id>.jsonl`), every line up to the end of the turn.
  transcript: { file: string | null; lines: unknown[] };
  // What couldn't be included, and why. A Codex session has no transcript the runtime keeps (ADR 24): its history
  // is in SQLite on the session's volume, so its bundle has no request ids, agent version or transcript.
  unavailable: string[];
};
```

A session starts from its first message: the UI opens an empty chat, and sending the first message creates the session. The runtime derives the session's repository from that message: an `owner/name` the user's GitHub App can access, or a bare repository name that matches exactly one of them. Without one, the session has no repository ([ADR 1](adr/0001-sessions-decoupled-from-directories.md)). Messages sent before the agent is up are queued and shown immediately.

```ts
type NewSession = {
  prompt: string;        // the first message; recorded right away and sent once the agent is up
  repo?: string | null;  // "owner/name"; derived from the prompt when absent; null for none (e.g. a suggested one the user cleared)
  title?: string;        // kept as given; without one, the agent's automatic title is used
  agent?: string;        // the agent to run: "claude-code" (the default) or "codex"
  capabilities?: string[]; // what it starts with, from "github", "packages" and, with a ChatGPT login, "codex"; all of them when absent. Model access and "docs" always come, memory with the user's memini, and "github" with a repository. 400 for anything else
  model?: string;        // instead of the user's default model
  grants?: StartingGrant[]; // the starting set as grants, instead of repo and capabilities (400 if both): see below
};

// One grant of a starting set (ADR 2, ADR 29). "model" and "docs" always come, and "github" with a repository.
type StartingGrant = {
  capability: string;    // one of the built-ins offered (github, packages, codex with a ChatGPT login), "repo", or one of the user's own capabilities by name
  params?: { repo?: string }; // for "repo": which repository
  credential?: string;   // for "repo" only: one of the user's github-user tokens that reaches it, to act as them there; else the GitHub App, which must reach it
};

// A suggested starting set (ADR 2, #232). Advisory: the UI shows it as a card the user edits, and sends what the
// card says with POST /api/v1/sessions, which checks it again.
type StartingSet = {
  title: string;
  repo: string | null;   // one the user's GitHub App reaches, as GitHub spells it, or none
  capabilities: string[];
  source: "judgement" | "match"; // a judgement (ADR 26) made it, or matching the message's words did (the fallback)
};

type Session = {
  id: string;            // UUID; also the ACP sessionId
  title: string;
  repo: string | null;   // from the session's repo grant, if any
  branch: string | null; // the session branch, e.g. "sloppod/3f2a…"
  grants: Grant[];
  phase: Phase;
  agent: string;         // "claude-code" or "codex"
  model?: string;        // the model it runs on, when one was chosen rather than the agent's default
  kind: "work" | "review" | "utility"; // "review": a session reviewing a proposal (ADR 24); "utility": an owner's session for the runtime's judgements (ADR 26); `GET /sessions` leaves both out
  review: "off" | "required"; // whether its pull requests are reviewed by another model first; from the user's settings
  created_at: string;    // RFC 3339
  updated_at: string;
  active_at: string;   // when something last happened (an event was recorded); `GET /sessions` is ordered by it, newest first
  deleted_at?: string;   // when it was deleted; `GET /sessions` leaves deleted sessions out
  agent_status?: string; // what the pod is doing while the agent starts, e.g. "cloning https://github.com/…"
  agent_error?: string;  // why the agent last failed to start or exited; it's started again meanwhile. Both clear once it's ready
  parent_id?: string;    // the session that spawned it, for a sub-session (ADR 27)
  read_seq?: number;      // the seq of the last event the user has read in this session, if they have (see "Read position" below)
  pending_model?: string; // a model the user picked that needs the agent started again (to or from an endpoint's, #461): the session moves onto it before its next message is sent (see "Endpoint models" below)
  limited_until?: string; // the session hit the account's usage limit and carries on by itself then (see "When a turn fails on the account's usage limit" below)
  commands: number | null; // how many of the agent's commands are running, foreground or background, as its supervisor last said (`null` until it says, e.g. no pod); an `idle` session with some running is waiting on them, not done (#413). Kept live, not stored: a dropped link makes it `null` again
  command_lines?: string[]; // those commands' command lines, each cut to 300 characters, when the supervisor sends them
  waiting: { proposals: number; questions: number; permissions: number; highlights: number }; // what waits on the user: proposals awaiting a decision, open questions (`ask_user`'s too), the agent's permission requests and highlights neither marked seen nor retracted (#439) and not snoozed until later (#467), so a list of sessions can say which need them (#368). Counted when the session is sent (proposals from the database, the rest kept live) and pushed as `_sloppod/session` whenever it changes
  unschedulable?: string; // why the cluster can't place the session's pod, once it has been unschedulable for 20 s (#454): "waiting for room (insufficient memory)" when it's short of resources, otherwise the scheduler's own message (a taint, a volume that can't attach, a quota). Absent while a pod is merely being placed. Kept live and pushed as `_sloppod/session` when it changes
  disk?: { used: number; size: number; seen_at: string }; // its workspace volume's use and size in bytes, as its supervisor last said (every 5 minutes), and when; kept while it's parked (#455)
  volume_kept: boolean; // whether the user keeps the volume of a stopped session, otherwise removed a week after it stopped
};

// What the session may do (ADR 2). Capabilities so far: "model", "github" (read: public data, plus the "repo"
// grants), "packages" (package registries), "codex" (Codex through the owner's ChatGPT login), "repo"
// {repo, access: "rw", branch, credential?, as?}, "model" and "codex" {credentials: string[]} (below), "memory" {namespace, access: "rw"}, "docs" (library documentation through Context7's MCP server, which the runtime serves at `/servers/docs` and agents get as `docs` tools; granted by default). `description` says what the grant allows in plain
// terms, in the same words the agent's `list_grants` tool uses.
type Grant = {
  capability: string;
  params: Record<string, unknown>;
  description?: string;
  id?: string;           // for changing or revoking it
  created_at?: string;   // when it was granted
  expires_at?: string;   // when it runs out, if it does
};

// Something the agent asked the user to see with `highlight` (below); open until the user marks it seen.
type Highlight = {
  id: string;
  session_id: string;
  text: string;          // Markdown
  created_at: string;
  seen_at?: string;      // when the user marked it seen; absent while open
  snoozed_until?: string; // until when the user put it off (#467); absent when it isn't snoozed, cleared when the runtime brings it back
  retracted_at?: string; // when the agent took it back (it was wrong, or no longer applies); no longer open, kept in the chat with the reason
  retraction_reason?: string;
};

type Phase =
  | "starting"           // pod being created or agent not connected yet
  | "idle"               // agent connected, no turn running
  | "working"            // a turn is running
  | "waiting"            // a turn is blocked on a permission request or a question
  | "disconnected"       // pod went away; the runtime is bringing it back
  | "parked"             // no pod (ADR 5); messages queue, and sending one wakes it
  | "stopped";
```

`POST /api/v1/sessions` derives the repository and grants before it returns. The owner changes a session's grants while it runs ([ADR 2](adr/0002-capabilities.md)): grant a capability it lacks, change when one runs out, narrow it, or revoke it. A grant that has run out or been revoked stops counting at once, wherever the session's grants are read (the authorizer reads them on every request); egress that depends on grants (direct access to package hosts with `packages`) is applied again, and the agent is told what changed. A minute's sweep removes grants that have run out and tells their agents. Memory can't be granted mid-session, since its key is minted when the pod is placed, and the docs MCP server granted mid-session reaches the agent at its next start. A session gains more repositories later through `POST /api/v1/sessions/{id}/repos`. Given a `credential`, the session acts on that repository with the user's own GitHub token instead of the GitHub App ([ADR 29](adr/0029-grants-name-their-credential.md)): the token has to reach it (one of the repositories it listed when it was added, or any when it listed more than a thousand), and the grant gains `credential` (its id) and `as` (whose it is). The authorizer then attaches the token for git reads and pushes there and for API reads of `/repos/<that repository>/…`; the App's tokens never cover it, a token that's gone is refused rather than replaced by the App, and every request made with it is logged with the session and credential. Pushes are still limited to the session's branches. Only the user does this. A sub-session inherits it only to read the repository and push its own branches: its grant keeps `credential` and `as` and gains `operations: ["read", "push"]` (a grant without `operations` covers everything), so its outward proposals there are refused, it gets no token on GraphQL there (see the narrow-token rule in ADR 29), and its report still goes through, for the parent to propose from; a reviewer of it reads the repository with the token, limited to `read`. Repositories also come when the agent requests one and the user approves (`sloppod.repo.grant` proposals). `repo` is the repository the session was started for, which is checked out as its working directory, or else the first one it was given; `branch` is its session branch, the same in every repository. Until the agent names the session, its title is the first line of the first message. It returns `409` when the user has no credential for the agent's model (a Claude account for Claude Code, a ChatGPT login for Codex), and `400` with `{error}` when a given `repo` isn't accessible or `agent` isn't one the runtime has.

**Endpoint models** ([#461](https://github.com/bo0tzz/sloppod/issues/461)): an `endpoint` credential is an Anthropic-compatible endpoint the user runs or uses, such as a LiteLLM proxy, with the models it serves. Each of its models is named `endpoint:<credential id>:<model>` wherever a model is chosen: settings' `models` lists them as `{value: <that id>, name: "<model> (<endpoint name>)"}`, so they're offered as the default model, as the model sub-sessions run on, and in a session's model selector; `POST /api/v1/sessions` and `spawn_session` take one by its id or, when only one of the user's endpoints serves it, by its bare name. Only Claude Code runs on them (`400`, or a refusal, for Codex). A session on one holds an `endpoint` grant naming that credential (`{credentials: [id]}`), which follows its model: it comes with the model, goes when the session moves off it, and is never granted on its own. A sub-session gets the one its own model needs, whether or not its parent holds it ([ADR 27](adr/0027-sub-sessions.md)). Such a session needs no Claude account. Its agent is pointed at the runtime's `/inference` route (below) with a placeholder token, and told the model's context window when its provider knows it (a LiteLLM operator model's `spec.info.maxInputTokens`, or the credential's `context_windows`), as `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, which its auto-compaction follows; otherwise Claude Code assumes its default window and says so, and the runtime sends its requests on to the endpoint with the endpoint's key, so the endpoint can be a cluster Service and the pod never holds the key. Moving a session to or from an endpoint's model (`session/set_config_option` with `configId: "model"`) is a change of provider ([ADR 20](adr/0020-switchable-inference-accounts.md)): so that picking one doesn't restart the agent under the user, the runtime checks the pick and records it as the session's `pending_model`, and responds `{}`; picking the current model again takes it back, and so does a change between the agent's own models, which the agent makes at once as usual. When the next `session/prompt` arrives, before it's sent, the runtime records the model and grant together and starts the agent again on a new pod, resuming its conversation, under the session's transition lock; the message waits for the new agent and then runs on the new model. A parked session just records them and starts on them when the message wakes it. A session that isn't quiet then (a turn running, something queued, an open request, a command running) keeps the pick for the next message that finds it quiet. If the move fails (provisioning a key, replacing the pod), every part of it is put back, the pick is dropped, the session stays on its old model and the message fails with why, unsent. Moving off an endpoint onto the agent's own provider needs its account (a Claude account) and grant, or the pick is refused. In the model selector, a session on an endpoint's model offers Claude's models (settings' `agent_models["claude-code"]`) and the endpoints', not the aliases its agent lists. An endpoint grant's credential can't be changed through `PATCH /grants` (`400`): its endpoint changes with the model. A user default naming an endpoint that's been deleted is ignored for new sessions, which start on the agent's default; a session whose endpoint was deleted is refused until it's moved onto another model. Usage-limit handling (below) is Claude's and ChatGPT's: an endpoint's refusals (a budget spent, a rate limit) fail the turn with the endpoint's error, and Claude Code's quota reports aren't kept for it. The runtime's judgements and reviewers keep their own models.

**LiteLLM operator models** (#461): where the deployment watches namespaces for the [LiteLLM operator](https://github.com/home-operations/litellm-operator)'s resources (`litellm.namespaces` in the chart, `SLOPPOD_LITELLM_NAMESPACES`: by default the runtime's own namespace, and an empty list turns it off; where the operator isn't installed, it says so once and looks again every 10 minutes), each `LiteLLMProxy` there is an endpoint too, offered to every user, with the `LiteLLMModel`s bound to it as the operator binds them: a model's `proxyRef` (and `proxyNamespace`), else the proxy's `modelSelector`, else every model in the proxy's namespace. They're named `endpoint:litellm/<namespace>/<proxy>:<modelName>`, shown as `"<modelName> (LiteLLM <proxy>)"`, and chosen like an endpoint credential's. A session on one holds an `endpoint` grant `{location: "litellm/<namespace>/<proxy>", model}`, and has a `LiteLLMVirtualKey` of its own in the runtime's namespace for that proxy, `sloppod-<session id>-<hash of the proxy's location>`: limited to its model, with `keyAlias` the same and `metadata` naming the session and its owner, its key in the Secret `<that name>-key` that the operator mints. A session moving to another proxy gets a key of its own there, and its old one is deleted with its own proxy reference. The runtime makes it when the session is placed (and updates it when the session moves to another of the proxy's models), reads the Secret when the session's first request comes (waiting up to 15 seconds for the operator), and attaches that key to the session's requests to the proxy's Service, `http://<proxy>.<namespace>.svc.<cluster domain>:<port>` (the proxy's `service.port`, 4000 by default); the pod never sees it. The virtual key is deleted when the session stops or is purged, or moves off the proxy, and the operator deletes the remote key and its Secret with it. A purge whose key couldn't be deleted is tried again by the next purge sweep, and an hourly sweep deletes any virtual key no live session holds (made over 10 minutes ago), e.g. one whose release after a move failed. A move between models that fails part way is put back whole, keys included. A model whose proxy or binding goes away can't be chosen any more, and a session on it is refused until it's moved onto another model.

**Which account a session uses** ([ADR 29](adr/0029-grants-name-their-credential.md)): a `model` grant names the user's Claude accounts in `params.credentials` (their ids, in order), and a `codex` grant their ChatGPT logins. When the grant is made (a new session's starting set, a sub-session's copy of its parent's, which keeps the parent's list, a grant from the grants list, a review session), it lists all of the user's accounts of the kind, the default first, and it keeps that list until the user changes it: making another account the default changes what new grants get, not what running sessions use. The authorizer attaches the first account on the list that still exists. A grant whose accounts were all deleted is refused, with a message saying so, until the user gives it another; one made before the user had any account of the kind uses their default. The user reorders, trims or extends the list with `PATCH /api/v1/sessions/{id}/grants/{grant}` and `credentials` (at least one, each one of their own of the grant's kind, none twice; `400` otherwise): putting another account first switches the session to it from its next request ([ADR 20](adr/0020-switchable-inference-accounts.md)), and a session waiting for the old account's usage limit waits for the new one's instead, or carries on at once if it isn't limited. The runtime's own judgements ([ADR 26](adr/0026-runtime-inference.md)) always use the user's default Claude account.

### Credentials

Accounts and keys the runtime uses on a user's behalf ([ADR 21](adr/0021-credentials-live-in-the-runtime.md)). Secrets are write-only: the API never returns them. The first credential of a kind becomes the default for it; deleting the default promotes the oldest remaining one. The default is what new grants get first (above) and what the runtime's judgements use; a running session uses the accounts its grants name. An invalid credential (unknown kind, a private key that doesn't parse, a URL without a host) gets `400` with `{error}`.

**Own capabilities** ([ADR 2](adr/0002-capabilities.md)): the user defines capabilities sloppod passes through without knowing what they are. So far that's an MCP server, e.g. their VictoriaLogs or VictoriaMetrics one. A definition is theirs alone, and is granted like a built-in, by its name, through `POST /api/v1/sessions/{id}/grants`; it's in no starting set. Its grant's description is the definition's. Like `docs`, a server granted while the agent runs reaches it at its next start (after a park and wake, say), since an agent's MCP servers are set when it starts; the agent is told so. The runtime serves it to the sessions granted it on the sessions port (`/servers/<name>`, below), so a credential for it never enters the pod.

```
GET    /api/v1/capabilities       -> Definition[]
POST   /api/v1/capabilities       -> Definition (201)  body: {name, description, mcp_url, credential_id?}; 400 for an invalid one (or one whose host is the runtime itself, loopback, unspecified or link-local), 409 for a name taken
DELETE /api/v1/capabilities/{id}  -> 204   grants of it stay, and stop working
```

```ts
type Definition = {
  id: string;
  name: string;           // 1 to 32 lowercase letters, digits and dashes, starting with a letter; not a built-in's name, "sloppod" or "memini"
  description: string;    // what it's for; the agent's list_grants and the grants list show it
  mcp_url: string;        // the server's http(s) URL, without credentials in it
  credential_id?: string; // a "token" credential, attached to every request
  created_at: string;
};
```

A `token` credential is a secret such a server takes: sent as `Authorization: Bearer <token>`, or, with `config.header`, as that header's value.

A `github-user` credential is the user's own GitHub identity, for acting as them where they grant it ([ADR 28](adr/0028-acting-as-the-owner.md), [ADR 29](adr/0029-grants-name-their-credential.md)): a session's repository grant can name one (`POST /api/v1/sessions/{id}/repos` with `credential`). Only fine-grained tokens (`github_pat_…`) are taken: a classic one (`ghp_…`) reaches everything its user can, so it's refused. When one is added, the runtime asks GitHub whose it is (`GET /user`), when it expires (the `github-authentication-token-expiration` header; `expires_at` is `null` for one that doesn't), and which repositories it reaches (`GET /user/repos`, the first thousand, with `more_repositories` when there are more), and keeps that as its config. A token GitHub refuses gets `400`. Like the other secrets, the token stays in the runtime, sealed (ADR 21).

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
  | "chatgpt"            // secret: the auth.json of a `codex login` only sloppod uses; config: {account_id} (derived)
  | "github-user"        // secret: a fine-grained GitHub personal access token; config: {login, expires_at, repositories, more_repositories} (derived)
  | "token"              // secret: a token an own capability's server takes; config: {header?: string} (sent as that header, else as a bearer token)
  | "endpoint";          // secret: the key of an Anthropic-compatible endpoint (#461); config: {url: string, models: string[], context_windows?: {[model]: number}} (models given as a list, or one string of names separated by commas or spaces; context_windows, each of its models' window in tokens, at least 1000, as an object or one string of `model=tokens` pairs; 400 for one that names no model of its or isn't a number)

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

### Quotas

What's left of the usage limits of the user's inference credentials, as last reported (#140). Claude Code reports a Claude subscription's limits in its sessions' `usage_update`s (`_meta["_claude/rateLimit"]`), under the Claude account last attached to the session's requests; they're known once a session has used it. Every ChatGPT login's are read from ChatGPT every five minutes, as the Codex CLI reads them, since codex-acp doesn't report them. Each report updates the windows it covers and keeps the others as they were; one that arrives after a newer one changes nothing the newer one said. A ChatGPT report that isn't as expected is logged and ignored. A quota reads as it stands now: a window whose reset has passed shows as unused, and a refusal ends at its `resets_at`; one reported without a reset reads as resetting 30 minutes after it was reported, when the account is tried again. Only credentials with a report are listed.

Changes are pushed rather than polled: every `/acp` connection of the owner, whatever session it has loaded or none, gets `_sloppod/quota` (below) when a quota changes: a report from Claude Code or ChatGPT, a turn hitting the limit, a reset passing (the runtime wakes at the earliest reset of any quota), or the credential being deleted. A ChatGPT login's limits are read as soon as it's added. A UI fetches `GET /quotas` once each time its connection comes up, to catch what changed while it was down, and keeps up from the notifications. The web UI shares one connection for all of this.

```
GET /api/v1/quotas -> Quota[]
```

```ts
type Quota = {
  credential_id: string;
  kind: CredentialKind;    // "claude" or "chatgpt"
  name: string;
  limited: boolean;        // the provider refuses its requests now, until resets_at
  resets_at: string | null;
  windows: QuotaWindow[];
  observed_at: string;     // when the last report came
};

type QuotaWindow = {
  window: string;          // "five_hour" (the session limit), "seven_day" (the weekly one), or another limit's name
  utilization: number | null;  // the fraction used, usually 0 to 1
  resets_at: string | null;
  status: "allowed" | "warning" | "rejected";  // warning: past a threshold the provider set; rejected: at the limit
};
```

**Spend** ([#461](https://github.com/bo0tzz/sloppod/issues/461)): what sessions on endpoint models have spent, as the endpoints say. An endpoint like LiteLLM prices every call and keeps each key's spend and budget, which the key itself reads (`GET /key/info`), so the runtime reads it every five minutes, for each key a session not stopped or purged uses, the way it reads a ChatGPT login's limits, and once more before a key goes (the session stops or is purged, or moves off the endpoint), and keeps each key's latest. A session on a LiteLLM operator model has a key of its own per proxy, so its spend is its own: the sum of its keys', if it moved between proxies or came back to one (each key minted is kept apart); an endpoint credential's one key serves every session on it, so its spend is the credential's. An endpoint without key info (one that isn't LiteLLM) has none to show. A session's figure stays after it stops; the user's total is the sum over every key their sessions used. When a figure changes, every `/acp` connection of the owner gets `_sloppod/spend` (below), for the UI to read it again. The user's `session_budget` setting is set as `maxBudget` on each new session's virtual key (and on a move to another of a proxy's models), which LiteLLM enforces: a session past it is refused by LiteLLM, and its turn fails with LiteLLM's error, as other endpoint refusals do.

```
GET /api/v1/spend -> Spend
```

```ts
type Spend = {
  total: number;         // dollars, over every key the user's sessions used
  keys: {
    key_id: string;      // "session:<id>" for a session's own keys, summed, or the shared key's, e.g. "credential:<id>"
    session_id: string | null;
    name: string;        // the session's title, or the shared key's name (an endpoint credential's)
    spend: number;
    max_budget: number | null;
    observed_at: string; // when it was last read
  }[];                   // most recently read first
};
```

### Proposals

Outward actions and access the agent asked for ([ADR 3](adr/0003-proposals-and-trusted-executor.md), [ADR 2](adr/0002-capabilities.md)). Kinds:

- `github.pr.create`: payload `{repo, base, head, head_sha, title, body, draft, auto_merge}`, proposed with an optional `review` (below); the detail adds `diff` and `commits`, up to `head_sha`, the commit that was proposed. Approving opens the PR only if `head` still points there; otherwise it fails and the agent proposes again. With `auto_merge`, which the agent can ask for and the reviewer decides, sloppod turns on GitHub's auto-merge after opening the PR (with the first merge method the repository allows), so it merges once its required checks pass, and only at the reviewed commit (`expectedHeadOid`); the result's `message` says whether that worked. It merges with the pull request's own title and description as the commit message (`commitHeadline` "<title> (#<n>)", `commitBody` its description without agents' credits), not GitHub's default, whose squash body lists every commit's author as a co-author, the App's bot among them; a rebase merge keeps the commits' messages. Every path that turns auto-merge on does this: opening a pull request or a stack, and turning it back on after a push. Opening it records a fingerprint of the change it makes (`fingerprint`: the files and added and removed lines of its diff against `base`, like `git patch-id`; none when binary files change). Pushing to a head with auto-merge on turns it off but lets the push through. Once the push has landed, the PR's diff against its base is fingerprinted again: if it matches, as after a rebase or a merge of the base, auto-merge goes back on, pinned to the new head; if not, including a resolved conflict, it stays off and the PR needs merging by hand. The session is told only in that case: auto-merge going back on is the expected outcome, with nothing to act on, and a notice would cost an idle session a turn. A head without a fingerprint, such as a stack's layer or a PR opened before fingerprints, still has its first push refused with a message, and needs merging by hand. Every notice that a pull request needs merging by hand ends with the next step: the agent can't merge, so it tells the user, with the link. An undecided pull request or stack whose base was another branch follows that branch when its pull request merges and GitHub deletes it, as GitHub retargets open pull requests (#141): every minute the runtime looks at proposals that are `pending` or `changes_requested`, and at each `request_review`, and one whose base is gone and merged (followed down a stack that merged from the bottom up) is prepared again against the branch it merged into. If it merged with a merge commit, what merged with it is in the new base's history and drops out of the diff by itself; squashed or rebased, it isn't, so the runtime merges the new base into the head (GitHub's merges API, a merge commit on the session's branch) to leave it out. On a conflict the base moves anyway and the session is told to merge it in itself. The session hears what moved, in words that hold even if it withdrew the proposal since. Following runs under the proposal's row lock, and only while it's still `pending` or `changes_requested` (at `request_review`, still `changes_requested`), whether the minute's check or a `request_review` follows it, with its update applied in the same transaction: a withdrawal or decision meanwhile waits for it, and a proposal withdrawn first gets no merge commit and no notice. Sessions are told up front, in their instructions and in `propose_pull_request` and `propose_stack`, that a proposal follows its base this way, so they don't withdraw and propose it again. A `pending` proposal that was reviewed goes through another round if its change, by fingerprint at the commits it now pins, isn't the one reviewed any more (a new reviewer, if the old one is gone, is given the last round's findings to account for); one with changes requested is reviewed as it now is at its next `request_review`. A proposal still `in_review` follows once its round is over, and one approved in the minute before it's followed fails to open as before.
- `github.stack.create`: pull requests that build on each other, e.g. branches managed with `gh stack`; payload `{repo, base, pull_requests: {head, base, head_sha, title, body, draft}[], auto_merge}`, proposed with an optional `review` like a pull request; bottom first, each based on the one below and the first on `base`. The detail adds `layers: {diff, commits}[]`, one per pull request. Approving checks that every head is still at its `head_sha` before opening anything, opens them (reusing open ones), and links them as a stack on GitHub; the result is `{pull_requests: {url, number}[], stack, message}`, with `stack` null and the reason in `message` if linking failed. With `auto_merge`, which the agent can ask for and the reviewer decides, sloppod turns on auto-merge for the top pull request at its proposed commit once the stack is linked, which merges the whole stack when its required checks pass; without a linked stack it stays off. Until the stack has merged, the runtime checks every 30 s that each layer is still at its proposed commit, and if one has moved, by any route, it turns auto-merge off and tells the session. That narrows, but doesn't close, the window in which a moved layer could merge: its required checks would have to pass between two checks, or while the runtime is down. The watch resumes when the runtime restarts. Edits: `{auto_merge?, pull_requests: {title?, body?}[]}`, the titles and bodies by position.
- **Watching.** Once a pull request from either kind is open, the runtime polls it every minute and tells its session what changed, as a notice: its checks failed (naming them) or pass again after failing, it conflicts with its base, it has new reviews or comments, or it was merged or closed, after which it isn't watched any more. What was last seen of each is kept (`pull_watches`), so a restart neither repeats nor misses a change. At the first look at a pull request, what's already wrong is told too (it conflicts, its checks failed, it was merged or closed), but not the reviews and comments it has so far. An answer from GitHub without the pull request isn't taken as one; polls of a pull request that keep failing are logged as warnings (after 3 in a row, then every 30), since its session isn't told anything meanwhile. A notice that comes while the session waits for a usage limit is held with its queue and given when it carries on. Checks that are running, and mergeability GitHub is still working out, don't count as changes. Auto-merge being turned off or back on isn't reported here: the push that causes it reports auto-merge staying off, and its going back on is silent (above). Notices name a pull request with its repository (`owner/name#n`), since a session can push to several.
- `sloppod.work.plan`: plan changes to a work graph ([ADR 30](adr/0030-work-items.md)) from a session whose `work` grant of it is at `plan`; payload `{graph, graph_name, changes: {item, title, action, parent_id?, after?}[], reason}`, `action` one of `move` (reparent and rank, as `POST /work/items/{id}/move`), `drop` and `reopen` (a dropped item back to open). Its target is the graph's id, so a standing approval for the session and graph approves them; the detail adds `current: {id, title, state, parent_id, rank}[]`, the items as they are now. Carried out change by change, recorded as the session's; one that fails stops the rest, and the error says which applied.
- `sloppod.repo.grant`: write access to a repository; payload `{repo, reason, branch}`; the detail adds nothing.
- `sloppod.repo.act_as_owner`: acting on a repository as the user, with one of their GitHub tokens ([ADR 29](adr/0029-grants-name-their-credential.md)), and write access to the session branch there; payload `{repo, reason, branch, credential, login}`, `credential` the first of the user's tokens that reaches the repository and `login` whose it is. The detail adds `tokens: {id, name, login}[]`, the user's tokens to pick from. Edits: `{credential}`, another token, which has to reach the repository when it's carried out. No standing approval covers it, since approving picks which of the user's identities the session acts as: creating one for this kind is refused (`400`), and its proposals always wait for the user. The result's `login` says whose token it ended up acting with.
- `sloppod.session.report`: a sub-session's report to its parent that its task is done ([ADR 27](adr/0027-sub-sessions.md)); payload `{parent, summary, title, repo?, base?, head?, head_sha?, review?}`, `title` the task's (the session's title unless the child gives one), the branch fields when its branch (the session branch, or the one named after it that the child gives as `head`) has commits on the parent's (or on the default branch, which it started from if the parent hadn't pushed), and `review` (`{verdict: "pass" | "changes" | "stalled", summary?, findings?}`) once its review is over; the detail is the diff, as for a pull request. Between review rounds the child can correct its title and summary with `request_review`'s `update: {title?, summary?}`. It isn't outward: once out of review (or straight away, unreviewed), the runtime approves it (`decided_by: "sloppod"`) and carries it out by telling the parent.
- `sloppod.code_review`: a session's request for a review of a branch, without proposing anything (#254): work in progress, a spike, or a branch it'll split up later. Payload `{repo, base, head, head_sha, focus}`, `head` being the session branch or one named after it, and `focus` what the session asks the reviewer to look at, or `null`; the detail is the diff. It's always reviewed, whatever the session's review policy, so it needs the user's ChatGPT login; the reviewer is told nobody decides from it, so `focus` isn't held to describing the change. Rounds go as for a pull request: findings go to the session, which pushes fixes and calls `request_review` (with `update: {focus?}`) or withdraws it. When the review ends, the session hears how, and the runtime approves it (`decided_by: "sloppod"`) and carries out nothing. With `whole: true` (#344) it reviews the tree at `head` as it is rather than a change: payload `{repo, base: null, whole: true, head, head_sha, focus, tree: {files, bytes, truncated}}`, `tree` being the size of the tree at `head_sha` as GitHub lists it (`truncated` when the listing stopped at 100,000 entries or 7 MB), and the detail is empty, having no diff. `base` can't be given with it. The reviewer is told it's reviewing the code, not a change, so its instructions for a change (findings on what the change introduces, a description to hold it to) give way, and `focus` says what matters most. A reviewer reads the checkout itself, so no size is refused; past about 1 MB of code (`READABLE_BYTES`, about a model context) it can't read it all in a round's 30 minutes, so it reads what matters most and says what it left unread, and the session is told so when the review starts. Later rounds go as for a change: `git diff` from the last round's commit.
- `github.issue.create`: issues to file in a repository the session can push to; payload `{repo, issues: {title, body, labels}[]}`; the detail adds nothing. Labels that don't exist yet are created. The result lists the issues filed, and any that failed.
- `github.issue.update`: changes to existing issues in a repository the session can push to; payload `{repo, updates: {number, comment?, close?, reopen?, title?, body?, add_labels?, remove_labels?}[]}`, with `close` one of `completed`, `not_planned` or `duplicate`; the detail adds `current: {number, title, body}[]`, each issue whose title or body an update replaces as it is on GitHub now (once, however many updates name it), so a UI can show the change as a diff. Each update posts its comment first, then relabels, then edits and closes or reopens. Labels that don't exist yet are created. The result lists the issues updated, and any that failed.
- `github.pr.update`: changes to pull requests that are already open, in a repository the session can push to, like issue updates; payload `{repo, updates: {number, comment?, close?, reopen?, ready?, title?, body?, add_labels?, remove_labels?}[]}`, with `close`, `reopen` and `ready` each `true` (`false` isn't a change). `close` closes without merging, and `ready` marks a draft ready for review; a pull request can't be merged this way, and an update can't both close and reopen, or close and mark ready. The detail adds `current` as for issue updates, from the pull request's issue. Each update posts its comment first, then relabels, marks a draft ready (not if it no longer is one), then edits and closes or reopens. The result lists the pull requests updated, and any that failed.

A proposal on a repository the session acts on with one of the user's GitHub tokens ([ADR 29](adr/0029-grants-name-their-credential.md)) is read and carried out with that token, in the user's name: its pull requests and issues open as them, and auto-merge is turned on as them. Its payload then has `as`, the token's login, added by the runtime when it's proposed, so a UI can say who it acts as. It's carried out only as that identity: if the session's grant of the repository has switched to another identity since, it fails and the agent proposes again. The runtime's watch of the pull requests it opened, and of a stack's layers, reads them with the same identity.

**Review first** ([ADR 24](adr/0024-automated-review-before-human-review.md)): when the session's `review` is `required`, or the agent proposes a pull request or a stack with `review: true`, and the user has a ChatGPT login, the proposal starts `in_review` rather than `pending`. Without the login, a required review is skipped, and asking for one is an error. The runtime starts a review session (`kind: "review"`, running Codex, with read-only access to the repository and the commit under review checked out, detached, each time its pod starts), which ends each round by calling its `submit_review` tool. A pass moves the proposal to `pending`, where the user sees it and standing approvals can match it. Changes requested move it to `changes_requested` and the findings go to the agent, not the user; the agent pushes fixes and calls `request_review` (with an `update` of the proposal's fields, e.g. a title and body that fit the change again, and replies to findings it disputes), which re-pins the proposal to its branch's new head and runs the next round in the same review session. After three rounds with changes requested, the proposal goes to `pending` anyway, with the open findings in its reviews. Between rounds the review session is parked (ADR 5), and the next round wakes it on the new commit. It's deleted once the proposal passes, reaches that cap or is withdrawn, and when the author's session is deleted its proposals still in review are withdrawn. A sub-session's report is the exception: when its review passes, its reviewer is parked and kept, so a pull request or stack the parent proposes with `from_sub_session` (the sub-session's id) carries that review over (#247). Then the kept reviewer reviews the proposal, told which commit it passed, and looks at what changed since and at the title and description, rather than reviewing the whole change cold. That works when the sub-session's latest executed report passed review, is of the same repository, and its reviewer is still kept; otherwise the review starts afresh, and naming a session that isn't the proposer's sub-session is an error. A proposal that carried a review over keeps its reviewer when it ends (passed, withdrawn, rejected), so proposing the same work again carries it over again; a reviewer that's being deleted is never carried over. A kept reviewer no proposal is reviewing is deleted a day after its last proposal was decided. The runtime parks a review session only when its agent has no commands running; one left up with nothing to review (no proposal in review, idle for 12 minutes) is parked by a sweep once they are done (or anyway, ending them, once it has waited on them 15 minutes more), whatever happened to the park meant to follow its verdict, and a retired one whose pod is still up is removed. A round that stalls doesn't keep the proposal in review: a reviewer that ends its turn without calling `submit_review` is reminded once, and if it does so again, or the round takes over 30 minutes, the proposal goes to `pending` unreviewed (the author is told, and no standing approval matches it). `GET /api/v1/proposals/{id}/reviews` lists the rounds.

When a proposal is decided or carried out, the runtime tells the agent: the notice is recorded as a `user_message_chunk` with `_meta.sloppod.notice: true`, and, for one a UI should show folded, a one-line `_meta.sloppod.summary` (messages and reports from other sessions; the agent gets the full text) and delivered as a new turn when the agent is idle, steered into a running turn once the agent has no tool call in flight and no permission request open (steering cancels either, [ADR 4](adr/0004-asynchronous-interaction.md)), or queued until the agent is up. Either way it's kept with the session's queue until the agent answers the request that carries it (the turn's, or the steer's). After a runtime restart, the supervisor reports the requests the agent still has open: what was kept and is among them isn't sent again, and the rest is. It can come twice only if a restart falls between sending it and noting what it was sent as. What the agent gets of a notice leads with a line giving the time in the owner's timezone, e.g. `[sloppod: it's 2026-10-27 16:40 CET]`; the recorded notice doesn't have it. Sessions whose turn a runtime restart cut off get a notice of the same kind once their agent is back, naming the build now running (`SLOPPOD_BUILD`, the chart's app version).

**Falling back to another account** ([#252](https://github.com/bo0tzz/sloppod/issues/252)): a session's model grant names the user's accounts in order (above), and the one in use is the first that isn't at its usage limit, as its quota says. When a turn fails on the usage limit, the account it used (the one last attached to its requests, even if a quota report has since moved the choice on) is kept as limited (until the reset its quota knows of, or for 30 minutes when none is known, after which it's tried again). If another account on the grant isn't limited, the session doesn't wait: the authorizer attaches that one from the next request ([ADR 20](adr/0020-switchable-inference-accounts.md)), and the session carries on at once, as below, told "this session's Claude account X hit its usage limit; carrying on with Y". Only when every account on the grant is limited does it wait, until just after the earliest reset among them; when any of them stops being limited (its quota changes, or the user changes the grant's accounts), it carries on then. A session whose grant names one account waits as before.

When a turn fails on the account's usage limit (claude-agent-acp's `errorKind: "rate_limit"` or its "You've hit your …" text, codex-acp's `codexErrorInfo: "usageLimitExceeded"`), the session carries on with its next account if it has one that isn't limited (above), and otherwise waits for the limit to reset rather than counting it as an ordinary failed turn. `limited_until` on the session says until when: just after the earliest reset of its accounts, as their quotas say, otherwise 30 minutes on. When one of those quotas changes, the wait follows: to a newly known reset, or to now once one of its accounts isn't limited any more. Meanwhile nothing starts a turn by itself: messages queue as they do behind a turn (shown in `_sloppod/queue`), also while the agent isn't up, without waking a parked session; notices queue too, and an agent that comes back keeps the queue waiting. Steering a queued message in (`_sloppod/steer_queued`) tries it now; while the agent isn't up, that ends the wait, carrying on with everything queued. At `limited_until`, the agent gets a notice to carry on where it left off, in one turn with everything queued. A turn that hits the limit again waits again, and one that ends well ends the wait. The wait survives a runtime restart.

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
  kind: "github.pr.create" | "github.stack.create" | "github.issue.create" | "github.issue.update" | "github.pr.update" | "sloppod.repo.grant" | "sloppod.session.report";
  status: "in_review" | "changes_requested" | "pending" | "approved" | "rejected" | "executed" | "failed" | "withdrawn";
  payload: Record<string, unknown>;  // per kind, see above
  result?: Record<string, unknown>;            // when executed; pull requests: {url, number}; issues: {issues: {title, url, number}[], failed: {title, error}[], message}
  error?: string;                              // when failed
  reason?: string;                             // when rejected
  created_at: string;
  decided_at?: string;
  decided_by?: string;                         // the user, "standing-approval:<id>", or "sloppod" for a kind that isn't outward
  review_session_id?: string;                  // the session reviewing it, or that did
  summary?: string;                            // what it does, in a line: its kind's own summary, as notices give it; a UI shows it for kinds it doesn't render
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

Approving returns as soon as the decision is recorded, with status `approved`; the proposal is carried out in the background and its outcome (`executed` or `failed`) follows as a `_sloppod/proposal` update, and in `GET /api/v1/proposals/{id}`. Once carried out, its outcome is recorded even if the database is briefly away: the write is tried again, waiting up to a minute between tries, rather than the proposal being left `approved` and carried out again later. A proposal still `approved` when the runtime restarts is carried out again on startup; executors don't repeat work already done: an open pull request from the same head into the same base is reused, and the issues and comments a proposal posts carry a hidden marker (`<!-- sloppod:proposal/<id>/<n> -->`, the proposal and the item's position in it), by which they are found and not posted again, however long ago and whatever else says the same. Approving with edits changes the payload first: `github.pr.create` takes `{title?, body?, auto_merge?}`; `github.stack.create` takes `{pull_requests?: {title?, body?}[], auto_merge?}`, the pull requests' edits by position; `github.issue.create` takes `{issues}`, the issues to file, so leaving one out drops it; `github.issue.update` and `github.pr.update` take `{updates}` the same way. `sloppod.repo.grant` takes no edits; `sloppod.repo.act_as_owner` takes `{credential}`. A proposal's status only moves forward (`in_review` and `changes_requested`, back and forth between review rounds; then `pending`, which goes back to `in_review` when a reviewed change moved by itself (#141); then `approved` or `rejected`; then `executed` or `failed`), so a UI can ignore a copy, e.g. from a slow refetch, that is behind the one it has. Undecided proposals, in review or not, can be withdrawn.

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

### Work

The owner's work graphs ([ADR 30](adr/0030-work-items.md)). Each graph holds items. An item can be part of one other item, its parent, and can block others. A part waits on what blocks the items it's part of, and an item on its parts. No edge may leave an item waiting on itself that way, through blocks, parts or inherited blockers: adding one that would is refused (`409`), as is making an item part of one of its own parts, and so is an edge between graphs (`400`). An item links the issues, pull requests, sessions and proposals that carry it out (`completes`) or relate to it (`related`). Every change is an event in the item's history, and a change can be undone from there. Owners see and change only their own graphs. The runtime also moves links as what they point at moves, recorded as `sloppod`'s changes: pull requests as the PR watcher sees them, issues and other pull requests polled every minute while their link is open, sessions by whether they act or have reported, and a proposal's pull request joins the items its session is linked to (ADR 30, 2026-10-29, stage 3). The owner can also set a link's `status` here.

```
GET    /api/v1/work/graphs                     -> WorkGraph[]
POST   /api/v1/work/graphs                     -> WorkGraph (201)  body: {name}; 409 if you have one by that name
GET    /api/v1/work/graphs/{id}                -> WorkGraph & {items: WorkItem[]}   items by rank
PATCH  /api/v1/work/graphs/{id}                -> WorkGraph        body: {name}
DELETE /api/v1/work/graphs/{id}                -> 204              with its items and history
GET    /api/v1/work/graphs/{id}/frontier       -> WorkItem[]       what's ready to start, in order (below)
GET    /api/v1/work/graphs/{id}/history        -> WorkEvent[]      oldest first
POST   /api/v1/work/graphs/{id}/items          -> WorkItem (201)   body: {title, description?, parent_id?, after?}
GET    /api/v1/work/items/{id}                 -> WorkItem
PATCH  /api/v1/work/items/{id}                 -> WorkItem         body: {title?, description?}
POST   /api/v1/work/items/{id}/state           -> WorkItem         body: {state}; 409 where the rules refuse it (below)
POST   /api/v1/work/items/{id}/move            -> WorkItem         body: {parent_id?, after?}; parent_id null for the top level, left out to stay
POST   /api/v1/work/items/{id}/blocks          -> WorkItem         body: {item}; this item blocks `item` (no-op if it does)
DELETE /api/v1/work/items/{id}/blocks/{other}  -> WorkItem         it no longer blocks `other`
POST   /api/v1/work/items/{id}/links           -> WorkLink (201)   body: {kind, target, role}; 409 if it links that already
PATCH  /api/v1/work/links/{id}                 -> WorkLink         body: {status}
DELETE /api/v1/work/links/{id}                 -> 204
GET    /api/v1/work/items/{id}/history         -> WorkEvent[]      oldest first: its changes, and those of the edges it blocks through
POST   /api/v1/work/events/{id}/undo           -> WorkEvent (201)  the inverse change; 409 if it's undone already, changed since, or the rules' own
```

```ts
type WorkGraph = { id: string; name: string; created_at: string };

type WorkItem = {
  id: string;
  graph_id: string;
  parent_id: string | null;
  title: string;            // 1 to 200 characters
  description: string;      // up to 20000; the details belong in a linked issue
  state: "open" | "active" | "done" | "dropped";
  rank: number;             // orders it among its siblings, lowest first
  links: WorkLink[];
  blocks: string[];         // the items it blocks
  blocked_by: string[];     // the items that block it
  created_at: string;
  updated_at: string;
};

type WorkLink = {
  id: string;
  item_id: string;
  kind: "issue" | "pull_request" | "session" | "proposal";
  target: string;           // owner/repo#n for issues and pull requests; the id of one of your sessions or proposals
  role: "completes" | "related";
  // open: under way (open, a session working, a proposal waiting); finished: done what it's for (merged, closed as
  // completed, reported done, carried out); abandoned: ended without that (closed unmerged or as not planned, rejected).
  status: "open" | "finished" | "abandoned";
  created_at: string;
};

type WorkEvent = {
  id: string;
  graph_id: string;
  item_id: string | null;   // null for a graph's rename; the blocked item for blocks and unblocks
  actor: string;            // the user who made it
  session_id: string | null;  // the session that made it, if one did
  // create, edit, state, move, block, unblock, link, unlink, link_status, rename
  kind: string;
  before: unknown | null;   // what it changed, as it was: e.g. {state}, {title, description}, {parent_id, rank}
  after: unknown | null;    // and as it became
  automatic: boolean;       // a state change the rules made, following links and parts
  undoes: string | null;    // the change this one undoes
  at: string;
};
```

**Placing items.** `after` names the sibling (an item with the same parent) the item goes right after. Null puts it first, and leaving it out puts it last. Ranks are numbers between their neighbours'. When there's no room left between two, the siblings are spread out again (`1, 2, 3, …`) without an event of their own.

**State.** Items start `open`. The rules move them as their links and parts change, and each such move is an `automatic` event:

- An item is `done` once every child is `done` or `dropped` and every completing link is `finished`. An abandoned completing link no longer counts. An item without completing links is done this way only if it has children.
- An item that the rules made done or active, and that nothing makes so any more, goes back to `open`: for instance when its completing link is removed or abandoned, its last part is moved away, or the session working on it ends. An item made done by hand goes back to `open` when it gains an unfinished part or a completing link.
- An item is `active` while a linked session is open (working), or a completing pull request or proposal is open.
- A `dropped` item stays dropped. Otherwise an item keeps the state it was last given by hand.

By hand, through `state`:

- An item can't be made `done` while a child is unfinished, or while it has completing links (the links decide that).
- An item the rules find done can't be reopened.
- An item that something is working on can't be moved back to `open`.
- Dropping, and taking an item back out of `dropped`, is always allowed.

**Frontier.** These items are ready to start:

- open items whose parts are all done or dropped;
- that nothing unfinished blocks, either directly or through an item they're part of;
- that aren't inside a done or dropped item.

They're ordered by rank: siblings by their own, and items under different parents by their ancestors' ranks first.

**Undo** makes the inverse change and records it as an event with `undoes` set. Undoing that event redoes the change.
- A change can be undone only while its effect is still there, so a later change isn't silently overwritten.
- Undoing an item's creation drops the item; items are never deleted, so their history stays.
- The rules' own state changes can't be undone: change the link or part that caused them.

## ACP at `/acp`

A WebSocket carrying [ACP](https://agentclientprotocol.com) JSON-RPC 2.0, one message per text frame. The runtime is the agent from the UI's point of view. ACP `sessionId`s are runtime session IDs. Sessions are created through REST, not `session/new`. Like any write through the REST API, a browser can only open it from sloppod's own pages: a request whose `Origin` is neither the scheme and host it was made to (`X-Forwarded-Proto` and `X-Forwarded-Host`, or `http` and `Host`) nor one of `SLOPPOD_ALLOWED_ORIGINS` gets 403. Clients that send no `Origin`, like `sloppodctl`, are unaffected.

UI → runtime:

| Method | Notes |
|---|---|
| `initialize` | Returns the agent's capabilities, including `_meta.steering`. |
| `session/load` `{sessionId, cwd: "/", mcpServers: []}` | Replays the latest of the session's history (up to 2000 updates) as `session/update` notifications, then responds. When there's older history, the response's `_meta.sloppod.historyBefore` says where it ends; ask for it with `_sloppod/history`. From then on the connection receives that session's live updates. Several sessions can be loaded on one connection. The web UI opens one `/acp` connection for the whole app, shared by the session list, the usage meters and the open session (it loads the session it shows and closes it again when it moves on), and fetches its state once whenever that connection comes up: nothing polls the server. It asks for the page before by itself as the reader reads up towards it — no button, and never on opening, since reading is what calls for it — a spinner at the top of the chat while one is on its way, "Start of the session" once there is no more, and a retry at the top if one can't be read; what the reader is looking at stays where it is as the page goes in above (measured on the row they are in, since rows off screen are laid out at a guess until they come near, [#446](https://github.com/bo0tzz/sloppod/issues/446)). |
| `session/close` `{sessionId}` | The UI is done with a session: its events and requests stop coming on this connection (what it left open waits in the runtime and comes with the next load). Responds `{}`. |
| `_sloppod/history` `{sessionId, before}` | The page of history before `before` (up to 2000 updates), oldest first: `{updates: SessionUpdate[], at: string[], before}`, where `at` holds when each update was recorded (RFC 3339, in step with `updates`) and `before` is where to ask for the page before it, or `null` once there's no more. For a loaded session. |
| `session/prompt` `{sessionId, prompt: ContentBlock[]}` | Starts a turn. The prompt can carry `image` blocks (base64 `data` and a `mimeType`) when the agent says it takes them (`initialize`'s `agentCapabilities.promptCapabilities.image`); the runtime passes them through as they are. The web UI scales an image down to 1568 px on its longest side, re-encodes one over 3.5 MB (the model API's 5 MB limit once base64-encoded), and sends at most 10 per message. Responds `{stopReason}` when the turn ends. While the agent isn't up (`starting`, `disconnected`) the prompt is queued: it's echoed right away, everything queued runs as one turn once the agent is ready, and each queued request responds when that turn ends. While a turn is running, the prompt waits behind it instead: it isn't echoed yet, it shows in `_sloppod/queue`, and everything waiting runs as the next turn when this one ends (echoed then). A prompt or steer that reaches the agent 30 minutes or more after it was last heard from, or on a later date (in the owner's timezone), leads with a line for the agent only, not echoed or recorded: `[sloppod: it's 2026-10-27 16:40 CET, 3 hours since the last message]`, in the owner's timezone. After a runtime restart, the gap counts from the session's `active_at`. Queued prompts and notices survive a runtime restart: they're kept until the agent answers the turn that takes them (or, after a restart, still has it open), then run once the agent is back (or wake a parked session); a prompt request pending across a restart gets no response, since its connection is gone, but its turn shows in the transcript. The same holds when the agent's link drops before the agent answers a turn: what the turn carries, including a prompt that started it directly, stays kept, and the next link says whether the agent has it; if not, it runs again, logged once. Its pending `session/prompt` requests respond with an error saying it's kept. With `_meta.sloppod.messageId` (a UUID), the message is echoed, and queued, under that id, so a UI whose connection dropped before the response can tell from the next `session/load` whether the message arrived, and send it again if not. A prompt whose `messageId` the session already has, recorded or waiting in the queue, doesn't run again: it answers `{stopReason: "end_turn", _meta: {sloppod: {alreadyReceived: true}}}` right away. A UI can't always tell from what it loaded, since it only has the latest of a long history. `_session/steering` takes the same. |
| `session/cancel` `{sessionId}` (notification) | Cancels the running turn. The runtime answers the turn's open permission requests and questions as cancelled (the client needn't, unlike plain ACP) and sends `_sloppod/permission_resolved` for them; `ask_user` questions stay open. Messages still queued for an agent that isn't up stay queued. |
| `_sloppod/steer_queued` `{sessionId, id}` | Moves a message waiting behind the running turn into it, as a steer (echoed with `_meta.sloppod.steering`), and responds once the agent has it; its `session/prompt` request then responds too. Until then the message is kept with the session's queue, though no longer shown in `_sloppod/queue`, and not echoed; a `session/prompt` sent again with its `messageId` meanwhile answers `alreadyReceived`. If the steer can't be delivered (the agent isn't up, its link drops, the agent errs), it errors, and the message goes back to its place in `_sloppod/queue`, its `session/prompt` request still waiting; it runs at once if the agent is idle by then. A runtime restart in between leaves it kept: delivered if the agent still has the steer open and takes it, otherwise back at the front of the queue. |
| `_sloppod/remove_queued` `{sessionId, id}` | Takes a message waiting behind the running turn out of the queue without running it, e.g. so the user can edit it, and responds `{prompt: ContentBlock[]}` with what it held. Its `session/prompt` request responds `{stopReason: "cancelled"}`, as does a `session/prompt` sent again with its `messageId` by a UI that lost its connection before hearing so, even across a runtime restart. Like `_sloppod/steer_queued`, it errors if the message isn't queued any more. |
| `_session/steering` `{sessionId, prompt: ContentBlock[]}` | Delivers a message into the running turn. Before the agent is up it joins the queue like a prompt and returns `{outcome: "queued"}`. Otherwise it responds once the agent has it, and is echoed (with `_meta.sloppod.steering`) then; until then it's kept with the session's queue, so a runtime restart in between delivers it rather than losing it. If it can't be delivered it errors and isn't echoed or kept, for the client to send again. |
| `session/set_config_option` `{sessionId, configId, value}` | Changes one of the session's config options from the load response's `configOptions`, e.g. `model`. Responds `{configOptions}`. The session keeps its model across restarts. A change to or from an endpoint's model (#461) is recorded by the runtime as the session's `pending_model` and made before its next message, starting the agent again on a new pod (see "Endpoint models"); it responds `{}`. |

Sessions always run in their agent's most autonomous mode, Claude Code's auto mode or Codex's full access: the sandbox and the session's grants bound what any tool call can reach. Mode isn't configurable. Claude Code's auto mode is told what the sandbox is (its `autoMode.environment` setting, from `auto_mode_environment` in `adapter.rs`): an unprivileged pod with default-deny egress, placeholder credentials, pushes only to the session's own branches, sloppod's tools as its own runtime, and `sloppod:` messages that aren't the user's approval. Its checks then judge a tool call against what the pod can reach, not against a developer's own machine.

A few shell commands that go wrong without the agent noticing are refused before they run, with what to do instead (#339): blanket staging (`git add -A`, `--all`, `-u`, `.`, `:/`, and `git commit -a`/`--all`), which sweeps build output and scratch files into commits; reading one file from GitHub (`gh api` of `/repos/<owner>/<name>/contents/…`, `/readme` or `/git/blobs/…`, `gh browse` of a path, and `curl`/`wget` of `raw.githubusercontent.com` or of github.com's `…/raw/…` and `…/blob/…`), which the agent clones the repository for instead; `pkill`, whose pattern matches the tool's own wrapper shell and kills it; `pgrep -f` as a wait condition (in an `until`/`while` loop, a background task or a monitor), which always matches the watcher's shell and never ends; and detaching inside a background task (`nohup`, `setsid`, `disown`, `&` without `wait`), which ends the task at once with nothing watching the real work. For Claude Code that's a `PreToolUse` hook on `Bash` and `Monitor` in its `settings.json`, running `sloppod-supervisor guard`, which reads the call and exits 2 with the reason to refuse. These prevent mistakes and aren't a boundary ([ADR 11](adr/0011-enforcement-at-the-pod-boundary.md)): the check reads the command roughly, and lets through anything it can't read. Codex has no such hook; its reviewers don't stage, commit or run background tasks, and a Codex work agent would need its own rules (its execution policy can forbid command prefixes like `pkill` and `git add -A`, but not the ones that depend on how the command is run).

Runtime → UI:

| Method | Notes |
|---|---|
| `session/update` | Standard ACP updates: `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `config_option_update`, compaction reports, and `notice` (`{severity, title, description, resume?}`, e.g. why the agent failed to start, recorded once per distinct failure, or which of its commands parking the session ended). A turn that was stopped (`Stopped`) or failed (`The turn failed`, with the error) is marked with one too, `resume: true` (#406): the web UI shows a Resume button on it while it's the last thing in the transcript and no turn runs, which sends "Carry on where you left off." as a message. A turn that hit the usage limit isn't marked: it carries on by itself. Prompts sent by any UI are echoed as `user_message_chunk` with a `messageId` per prompt, so every viewer sees them; steers carry `_meta.sloppod.steering: true`, and notices to the agent (see Proposals) `_meta.sloppod.notice: true`. |
| `session/update` `_meta.sloppod.seq`, `_meta.sloppod.at` | On every `session/update`, live and in the replay of `session/load`: the update's place in the session's log, which a read position counts in (above), and when it was recorded (`session_events.created_at`, RFC 3339 to the second, in UTC). The web UI shows a message's time on hover, as HH:mm in the user's timezone setting (with the weekday when it isn't today and the date from a week on), from the first chunk of the message. |
| `session/request_permission` (request) | Sent to every UI that has the session loaded, including requests still open when a UI loads the session (after the `session/load` response). The first answer wins. |
| `elicitation/create` (request) | The agent asks the user something, e.g. with `ask_user` (below). Delivered like a permission request. Params: `{sessionId, mode: "form", message, requestedSchema, toolCallId?}`; the schema is an object of primitive fields (below). Answer `{action: "accept", content}`, `{action: "decline"}` (skip: the agent is told the user didn't answer) or `{action: "cancel"}`. |
| `_sloppod/queue` `{sessionId, messages: {id, prompt}[]}` | The messages waiting behind the running turn, sent whenever that changes and when a UI loads the session. |
| `_sloppod/permission_resolved` `{sessionId, toolCallId, requestId}` | Sent to every UI, including the one that answered, once a permission request or question is answered or cancelled. `requestId` is the JSON-RPC id the request had on this connection. |
| `_sloppod/quota` `{credentialId, quota: Quota \| null}` | One of the owner's quotas changed (see Quotas): `quota` as it stands now, or `null` once the credential is gone. Sent on every connection of the owner, with or without a session loaded. A connection that falls behind is dropped, so its UI reconnects and fetches them all. |
| `_sloppod/spend` `{}` | What the owner's sessions on endpoint models spent changed (see Spend): read `GET /api/v1/spend` again. Sent on every connection of the owner, whatever session it has loaded. |
| `_sloppod/session` `{session: Session}` | The session's phase, title, repository or grants changed, the agent's running commands (`commands`, `command_lines`), or what waits on the user (`waiting`: a proposal made, decided, withdrawn or carried out, a question asked or answered, a permission request opened or resolved). Sent on every connection of the owner for any of the owner's sessions, with or without it loaded, so a session list follows them without fetching; a connection that falls behind is dropped, so its UI reconnects and fetches the list. A connection that has the session loaded may get it twice. |
| `_sloppod/proposal` `{proposal: Proposal}` | A proposal for a loaded session was created or changed. |
| `_sloppod/highlight` `{highlight: Highlight}` | The agent highlighted something for a loaded session, or the user marked one seen or put one off until later, or the runtime brought a snoozed one back (so another tab shows it). Any of these also changes the session's `waiting.highlights`, which `_sloppod/session` carries to every connection of the owner. |

### Work done

`GET /sessions/{id}/work-done` is the log a session's right sidebar shows ([#205](https://github.com/bo0tzz/sloppod/issues/205)): what its carried-out (`executed`) proposals did to which issues and pull requests, from their results. One entry per item, newest (by when the last thing was done to it) first:

```ts
type WorkDone = {
  repo: string;            // owner/repo, lower case
  number: number;
  kind: "pull_request" | "issue";
  title?: string;          // as a proposal gave it (a pull request opened, an issue filed, a retitle); absent for an item only updated without a retitle
  url: string;
  did: string[];           // in order, without repeats: "opened" (a pull request or a stack's), "filed" (an issue), then what updates did: "commented", "closed", "reopened", "marked ready", "retitled", "body replaced", "relabelled"...
  at: string;              // when the last of it was carried out
  state?: { state: "open" | "merged" | "closed"; checks?: "success" | "failure" | "error"; failing?: string[] };  // a pull request the runtime watches, as it last saw it (`pull_watches`, Proposals above)
};
```

### Read position

Where the user has read up to is kept per user and session in the runtime ([#48](https://github.com/bo0tzz/sloppod/issues/48)), so it follows them between a browser and a phone. It's counted in events: each `session/update` carries its place in the session's log as `_meta.sloppod.seq` (live and in the replay of `session/load`). A UI reports the seq of the last event it has scrolled past, the bottom of what's on screen, with `PUT /sessions/{id}/read` (the web UI a moment after scrolling, not on every scroll); the runtime keeps the highest it's told, and when it moves pushes the session (`read_seq`) as `_sloppod/session` to all of the user's connections. The web UI marks the first item after it with an "Unread" bar while there's something unread, opens a session at that bar with a little context above it (at the bottom when nothing is unread), and keeps the bar where it was while the reader is scrolled up. Older history (`_sloppod/history`) carries no seqs; the UI takes it as read. A page that isn't being looked at reports nothing: while it is hidden (`document.visibilityState` isn't `visible`) or its window isn't focused (`document.hasFocus()` is false), the web UI still follows the bottom as output comes in, but what scrolls past counts as unread, and the bar marks where it was left. When the reader is back, the bar stays for a moment (1.5 s) before what's on screen is reported as read, so a glance doesn't mark a session read on every device. Focus counts as well as visibility because a window behind another is still visible; a reader who watches one window while typing in another isn't counted until they click it.

### Questions

Agents ask the user questions with sloppod's `ask_user` tool, which doesn't wait for the answer ([ADR 4](adr/0004-asynchronous-interaction.md)); Claude Code's own `AskUserQuestion`, which would stop the turn, is turned off. The runtime sends the question to UIs as an `elicitation/create` request like any other, but nothing waits on it: the session doesn't go `waiting`, a cancelled turn or an agent reconnect leaves it open, and the answer reaches the agent as a notice (`sloppod: the user answered your question: …`), or that it was dismissed; either names the questions. Either kind arrives as one form with two fields per question, in order: `question_<n>` and `question_<n>_custom`. Each carries its own id in `_meta.sloppod.questionId`, the same every time it's sent, so a UI can tell a question sent again (after a reconnect, say) from a new one that looks the same. An agent takes back a question that no longer applies (the user answered it in the chat, or it decided otherwise) with `withdraw_question {id, reason}`, `id` being what `ask_user`'s result named (`Asked (question <id>)`; the same id as `_meta.sloppod.questionId`): the question closes for the UIs (`_sloppod/permission_resolved`), stops counting in `waiting.questions`, is forgotten (it isn't restored after a restart) and the chat shows a notice, `Withdrawn: <reason>`. The agent is told nothing, as nothing answers it; withdrawing a question that was answered or withdrawn already is an error. Questions not yet answered survive a runtime restart: they're kept until answered and shown again once the runtime is back, like an agent's open permission requests, which the agent itself keeps open across the restart.

- `question_<n>` is `{type: "string", oneOf: [{const, title, description?}]}` for a single choice, or `{type: "array", items: {anyOf: [{const, title, description?}]}}` for several. `title` is the question's short header; `description` holds the question text when there are several questions (with one, it's the `message`). An option's `_meta["_claude/askUserQuestionOption"].preview` may carry a preview (Markdown/code).
- `question_<n>_custom` is a free-text "Other" answer: for a single choice it's the answer when nothing is picked, or a note on the pick; for several it's added to them.
- Nothing is required. `content` maps field keys to the picked `const` (or array of them) and the typed text.

Other form elicitations (from MCP servers) use the same primitive field types: `string` (optionally with `oneOf`/`enum`), `number`, `integer`, `boolean`, and arrays of enums.

### Highlights

An agent puts something in front of the user with sloppod's `highlight {text}` tool instead of writing it as an ordinary reply ([ADR 4](adr/0004-asynchronous-interaction.md)): the answer to something they asked, something they have to do that isn't a decision, or a serious finding they didn't ask about. `text` is Markdown, a few lines, up to 2000 characters. The call returns at once. The agent can take a highlight back with `retract_highlight {id, reason}`, `id` being what `highlight`'s result named (`Highlighted (highlight <id>)`), when it turns out wrong or no longer applies (if it changed: retract it and highlight the new state): `retracted_at` and `retraction_reason` are set, it leaves the "for you" list and `waiting.highlights`, and its card stays in the chat, muted, saying `Retracted: <reason>`; the change is pushed like seen is (`_sloppod/highlight`). Only the session that made it can, and only while the user hasn't marked it seen: retracting a seen one is an error that says the user has already seen it, as is retracting one twice. A highlight is kept (it survives a runtime restart and a reload) and stays open until the user marks it seen in the UI (`POST /sessions/{id}/highlights/{highlight}/seen`), however much the agent has done since; marking it seen starts no turn, and the agent hears which ones the user has seen with its next turn rather than at the moment ([#462](https://github.com/bo0tzz/sloppod/issues/462)): one note, `sloppod: the user has seen your highlight <id>: <its first line>`, leading whatever is sent next — the user's next message, a notice, a steer — with the lines of any others that were seen meanwhile. Keeping that note is the session's queue's doing: it waits there, and so through a restart, but it starts no turn and wakes no parked session by itself. The user can put one off instead ([#467](https://github.com/bo0tzz/sloppod/issues/467)) with `POST /sessions/{id}/highlights/{highlight}/snooze` `{until}`, an RFC 3339 time: while that's ahead it stops counting in `waiting.highlights`, so the session isn't waiting on them for it. The runtime's own check (every minute, and at startup) brings it back at that time: `snoozed_until` is cleared and it's pushed as `_sloppod/highlight` and `_sloppod/session` like any change to what waits, still without starting a turn or telling the agent. Snoozing isn't seeing it — a snoozed highlight counts as not seen, so the agent can still retract it — and there's no unsnooze: it is either marked seen or comes back by itself. The UI's choices (an hour on, this evening at 18:00 — tomorrow evening, and said so, once 18:00 has gone — tomorrow at 09:00, next Monday at 09:00) are worked out in the user's timezone from Settings ([#253](https://github.com/bo0tzz/sloppod/issues/253)), as the UI holds it since reading or saving the settings, so a timezone chosen since is the one it uses, and it keeps snoozed ones in a collapsed "Snoozed (N)" group in the sidebar under what waits, where each shows its text, "until …", Seen and Snooze again, the time read in that same zone — as it is on the card in the chat, which says "Snoozed until …". Any number can be open at once, and the session counts as waiting on the user ([#368](https://github.com/bo0tzz/sloppod/issues/368): `waiting.highlights`) while any is. A UI lists them with `GET /sessions/{id}/highlights`, hears of new ones and of those marked seen, put off or brought back as `_sloppod/highlight`, and shows each as a card where the tool call is in the chat, and gathers the open ones in a "for you" list. The call's result, which the agent's own tool call carries in the transcript, reads `Highlighted (highlight <id>). …`: that id is what ties the card to the highlight, since the runtime's tool handler isn't given the agent's tool call id (a call whose result carries no id, from an agent that doesn't pass results on, is matched by its `rawInput.text` to the first highlight of that text not matched yet). Only a session the user talks to has the tool: sub-sessions ([ADR 27](adr/0027-sub-sessions.md)) and reviewers don't, and the prompt's section on when to use it goes only to the former.

## Session pods

Every agent is told about its session in a prompt the runtime writes (`session_prompt` in `adapter.rs`): appended to Claude Code's system prompt, and in Codex's `AGENTS.md` after the owner's instructions. It has sections by what the session is for (`SessionRole`):
- **Every session:** its environment (the checkout, the network boundary, mise, and that another repository is read by cloning it shallowly into `$TMPDIR` rather than fetching single files or lines from it), and what runtime messages are. A message from the runtime starts with `sloppod:`, as notices, answers, relayed messages and review rounds do. It reports what happened and isn't the user's approval of anything, except that `sloppod: the user answered your question: …` carries the user's own answer, which the agent acts on as their decision on what it asked.
- **Work sessions and sub-sessions** (those that act) also get: their branches; proposals; working asynchronously ([ADR 4](adr/0004-asynchronous-interaction.md)); delegating (built-in subagents only for small things, where the agent has them, and sub-sessions for parallel work); and feedback on sloppod, where the agent mentions what got in its way.
- **A sub-session** is also told its parent's id: that its chat replies only reach the user, that `message_session` reaches its parent, and that `report_done` hands its work back.
- **A work session of its own** (not a sub-session) is also told when to use `highlight`: for the answer to what the user asked, something they have to do that isn't a decision, or a serious finding they didn't ask about; not for progress, plans or what a proposal or question already shows; read out of context, so it says what it answers, leads with the conclusion, stays short and links what it refers to. A highlight shows in the chat where it's called, like a message of the agent's, so the agent isn't to write its content in a reply before or after it: a reply around it adds only what isn't in it (the tool's description says the same), and that a highlight that turns out wrong or no longer applies is retracted (and the new state highlighted, if it changed), and a question that no longer applies is withdrawn.
- **A review session** gets the environment and messages sections, with a read-only checkout, plus the reviewer's own instructions.

Session pods use four more endpoints, on their own port (8081, `sessions`); that port is the only one they can reach, so they can't use the API above. Keep it off public routes.

- `/link/v1`: the supervisor link ([ADR 17](adr/0017-in-pod-supervisor.md)), a WebSocket authenticated with the pod's projected ServiceAccount token. Its types are in `crates/protocol`.
- `/mcp`: an MCP server (streamable HTTP, JSON responses) registered with the agent at session start. It speaks the 2025-era protocol — 2024-11-05 through 2025-11-25 — and 2026-07-28 ([#480](https://github.com/bo0tzz/sloppod/issues/480)), from how a request opens: one carrying the 2026 `_meta` (`io.modelcontextprotocol/protocolVersion`, with `clientCapabilities`) is served statelessly — the version also comes in the `MCP-Protocol-Version` header, the method in `Mcp-Method` and a `tools/call`'s name in `Mcp-Name`, and a header that doesn't say what the body does is refused (`-32020`) before anything runs; every result carries `resultType: "complete"` and sloppod's `serverInfo`, `server/discover` answers with the versions sloppod supports and its capabilities, and tool-list changes come on a `subscriptions/listen` stream (acknowledged first, each notice tagged with the subscription's id) — while an `initialize` request, and the requests that follow it, get the 2025 grammar: `initialize` answers with the version sloppod serves (a request for one it doesn't gets sloppod's newest of that era, and the client decides whether it can go on), and `GET /mcp` is the stream the changes arrive on. A 2026 request for a version sloppod doesn't serve is refused (`-32022`) with the list it does, and one missing its `_meta` is `-32602`. Its tool list follows the session's grants (a `work` grant adds the work tools, say): it says `tools.listChanged` when initialized, and a grant change (added, changed, revoked or run out) that makes the session's tools differ from the list it was last given is told on that stream (or subscription), so the client lists them again without the agent restarting (#323); a change made while the client had no stream open is sent as soon as it opens one. Claude Code does; the bundled Codex only with its under-development `mcp_tool_list_changed` feature, which sloppod leaves off, so a Codex session sees new tools at its next start. It identifies the session by the pod's own identity: the supervisor's local proxy attaches the pod's ServiceAccount token; pods from before that are identified by their IP. Its tools:
  - `propose_pull_request {title, body, repo?, base?, head?, draft?, auto_merge?, review?, from_sub_session?}` creates a `github.pr.create` proposal from the session branch, or from `head`, a branch named after it (`<session branch>-<name>`, the name of letters, digits, `.`, `_` and `-`), so a session can have several open; `repo` is needed when the session can write to more than one. The base defaults to the repository's default branch. A base that's a session branch the work started from (the session's own, or for a sub-session the parent's it branched off) and is behind the default branch with nothing of its own gives way to the default branch ([#373](https://github.com/bo0tzz/sloppod/issues/373)), so commits already there never show as the change's; the same holds for a stack's base, a report's and a code review's.
  - `propose_issues {issues: {title, body?, labels?}[], repo?}` creates a `github.issue.create` proposal (up to 50 issues; `repo` as for pull requests).
  - `propose_issue_updates {updates: {number, comment?, close?, reopen?, title?, body?, add_labels?, remove_labels?}[], repo?}` creates a `github.issue.update` proposal (up to 50 updates), e.g. to close issues the session's work finished, or merge duplicates. `close` is `completed`, `not_planned` or `duplicate`; an update with any other field, or one of the wrong type, is refused.
  - `propose_pull_request_updates {updates: {number, comment?, close?, reopen?, ready?, title?, body?, add_labels?, remove_labels?}[], repo?}` creates a `github.pr.update` proposal (up to 50 updates), e.g. to close a draft another pull request replaced, or mark a draft ready; `close`, `reopen` and `ready` are `true`. Merging isn't among them.
  - `request_repository {repo, reason}` creates a `sloppod.repo.grant` proposal; a sub-session can't, as it works only where its parent can push, and is told to say in its report that it needs the repository. A refusal for a repository the session can't push to, and `list_grants`, point to this tool only where the session can use it.
  - `request_owner_identity {repo, reason}` creates a `sloppod.repo.act_as_owner` proposal, to act on a repository with one of the user's GitHub tokens ([ADR 29](adr/0029-grants-name-their-credential.md)); refused for a sub-session, when the user has no token, when none reaches the repository, or when the session already acts as the user there.
  - `get_proposal {id}` returns a proposal's status and result. The agent is also told when its proposals are decided.
  - `highlight {text}` (see Highlights, above) puts text in the user's "for you" list and as a card in the chat; not for sub-sessions or reviewers. `retract_highlight {id, reason}` takes one back while the user hasn't marked it seen; `withdraw_question {id, reason}` takes back a question asked with `ask_user` (every session that has `ask_user` has it).
  - `withdraw_proposal {id, reason?}` withdraws one of the session's pending proposals: it becomes `withdrawn` (with the reason) and nothing is carried out.
  - `spawn_session {task, title?, repo?, grants?, agent?, model?, workspace?}` starts a sub-session ([ADR 27](adr/0027-sub-sessions.md)) with `task` as its first message, and answers in a sentence with its id, title and branch. It has the session's grants, or only the capabilities named in `grants` (model access always stays), and the session's owner, review policy and, unless given, agent. Its model is `model` if given, else the user's `sub_session_model` setting (above): by default the agent's cheaper model, Sonnet for Claude Code; the answer names it. `workspace` is `"copy"` or `"fresh"` ([#478](https://github.com/bo0tzz/sloppod/issues/478)): `copy` starts the child's volume as a clone of the parent's — its checkout, tools, dependencies and caches — which the parent's storage class has to support (docs/deploy.md), and by default a child working on the parent's repository gets one, while one working elsewhere starts fresh; the answer says which it got, including when a copy the parent asked for couldn't be made. Either way the copy carries nothing of the parent's: before its agent starts, the child's supervisor puts the checkout on the child's own branch from the parent's pushed head, without the parent's uncommitted or untracked files or its stashes, and clears what the parent left of its agent state, transcripts and keys. A clone that never provisions is replaced by a fresh volume a couple of minutes in, and the child is told. It works in `repo`, one the session can push to (by default the session's), on `<session branch>-<first 8 of its id>`, which starts from the session branch as pushed (from the default branch if it isn't pushed yet). A tree of sessions has at most 5 sub-sessions running at once (the tool's description says the same, from the same constant). A repository the session acts on as the user, the child acts on as them only to read it and push its own branches (above). The answer warns of any repository grant whose identity can't reach it: the GitHub App not installed there, or a token that doesn't list it. Work sessions only.
  - `list_work {graph?, view?, item?}` reads a work graph the session holds a `work` grant of (ADR 30): its items (`graph`, the default), what's ready in order (`frontier`), or one item with its description and history (`item`). `update_work {graph?, action, …}`, from `track`, adds and edits items, moves them between open, active and done, adds and removes blocks, and links and unlinks issues, pull requests, sessions (this one by default) and proposals, applied at once and recorded as the session's, so the user sees and can undo them. Ranking, reparenting, dropping and reopening are `plan` changes: `propose_work_plan`, shown only at `plan`, makes a `sloppod.work.plan` proposal. A session without a `work` grant gets none of these. A sub-session's grant of its parent's graph is at most `track`. A session holding a `work` grant, or linked to items, is told the graphs and those items in its prompt, and that the frontier is one call away.
  - `list_sessions` lists the session's sub-sessions: `{id, title, phase, model, repo, branch, doing, commands, command_lines}` each, `doing` saying what it's up to in words (an idle one with commands running is "waiting on 2 commands it started", one with none "idle, with nothing running", one whose pod can't be placed says why, as `unschedulable` does).
  - `message_session {id, text}` sends a message to the session's parent or one of its sub-sessions, delivered as a notice; a parked sub-session is woken for it unless its tree is at the limit, in which case it's refused.
  - `park_session {id}` parks one of the session's own sub-sessions (any other id is refused, as is one in a turn), so it stops counting towards its tree's limit; a message wakes it as above. A sub-session also parks by itself once it has finished: a report of it was carried out, nothing of its is in review, has changes requested, or is pending or approved, and it has been idle for 5 minutes with nothing queued, no open request, and its supervisor saying none of its commands are running. That's checked every 2 minutes.
  - `report_done {summary, title?, head?, review?}`, for a sub-session, creates a `sloppod.session.report` proposal: reviewed like a pull request under the session's review policy, then delivered to the parent with the summary, the branch and how the review ended. `head` names the branch the work is on when it isn't the session branch, e.g. a reused child's later task on `<session branch>-<name>` (the same rule as `propose_pull_request`'s `head`); a named branch without commits on the base is refused. The base is the parent's branch the child started from, or the default branch when that was never pushed or is behind the default branch with nothing of its own (as for pull requests, above). `title` says what the task was, for a child reused for more than one.
  - `request_code_review {head?, base?, repo?, focus?, whole?}` creates a `sloppod.code_review`: a review of a pushed branch (the session branch by default) against `base` (the default branch by default), or with `whole` of the whole tree at `head`, with nothing proposed. Its success text says so: no human is asked and nothing is carried out. For a whole tree it gives the tree's size, and says when that's more than the reviewer reads in one go. `request_review` takes it for the next round, with `update` limited to `{focus}`.
- `/memini/*`: memini, for sessions with a `memory` grant, identified like `/mcp`. The runtime forwards each request to the owner's memini with the session's own key and namespace, and refuses memini's administration endpoints. The memini plugin's `MEMINI_BASE_URL` points here.
- `/inference/*`: the Anthropic-compatible endpoint a session's model is on (#461), for sessions holding an `endpoint` grant, identified like `/mcp`. Only `POST /v1/messages` and `/v1/messages/count_tokens` are sent on, to the endpoint's URL with the path below `/inference`, with the endpoint's key as `authorization: Bearer` and `x-api-key` in place of the agent's placeholder; anything else, or a session without the grant or whose endpoint is gone, gets `403`. The request is read whole (up to 64 MiB); the answer streams back as it comes, with no limit on how long it takes, only on how long it goes quiet (10 minutes). Like `/servers/<name>`, it never reaches the runtime itself or a local address, and drops headers that say who or where a request is from. While the runtime is stopping it answers `503` with `retry-after`, and lets answers in flight finish (see docs/deploy.md). Claude Code's `ANTHROPIC_BASE_URL` points here.
- `/servers/<name>` and below: the MCP server of a capability the user defined, or of a built-in that's one (`docs`, Context7's), for sessions granted it, identified like `/mcp`. The runtime forwards each request to the definition's `mcp_url`, or the built-in's (the bare path to the URL itself, paths below it appended), with the definition's `token` credential attached (a built-in's gets none) and the agent's own `authorization` and `cookie` headers dropped, and streams the answer back. It never goes back to the runtime itself, by its pod or Service address, or to a loopback, unspecified or link-local one: names are resolved for each connection and those addresses left out, so `502` if that leaves none. Redirects come back to the agent rather than being followed. A session without the grant gets `403`; a definition deleted, or deleted and made again under its name since it was granted, gets `404` or `403`. Each request is logged with the session and the server's name. The agent's MCP config lists each granted one under its name.

None of `/memini/*`, `/servers/<name>` and `/inference/*` passes on headers that say who or where a request is from: `Forwarded`, `X-Real-IP`, any `X-Forwarded-*`, and the user header (`SLOPPOD_USER_HEADER`). The request comes from the runtime, so the agent could otherwise name any user to a service that trusts them.

A session that's stopped, parked or deleted can't act, even if its pod is still up: the authorizer refuses its egress, `/mcp` gives it no tools, and `/memini/*` answers 403.

In the pod, the repository is checked out at `/workspace/repo` on the session branch. `/workspace` is the session's own volume, kept across pod restarts.
