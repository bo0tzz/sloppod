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
POST   /api/v1/sessions/{id}/grants -> Session (201) body: {capability, expires_at?}; grants model, github, packages, docs or codex, or one of the user's own capabilities by name (below); 409 if the session has it
PATCH  /api/v1/sessions/{id}/grants/{grant} -> Session  body: {expires_at?: string | null, narrow?: true}; changes when it runs out (null: never), or narrows it (a repository it can push to becomes read only)
DELETE /api/v1/sessions/{id}/grants/{grant} -> Session  revokes it
PUT    /api/v1/sessions/{id}/review -> Session     body: {review: "off" | "required"}
GET    /api/v1/repos                -> string[]    repositories sessions can be granted, e.g. ["bo0tzz/sloppod"]
GET    /api/v1/me                   -> {user: string, version: string | null, build: string | null}   version: the release, e.g. 0.1.2-main.153 (SLOPPOD_VERSION, the chart's version); build: the commit or tag it was built from (SLOPPOD_BUILD, the chart's app version)
GET    /api/v1/settings             -> Settings
PUT    /api/v1/settings             -> Settings    body: {model?: string | null, review?: "off" | "required", timezone?: string}; fields left out stay as they are; 400 for a timezone that isn't one
```

```ts
// The user's defaults for new sessions. `models` is what the agent offered in the user's most recent session
// (empty before the first one); `model` null means the agent's default. `review` is new sessions' review policy.
// `timezone` is the user's, as an IANA name (e.g. "Europe/Amsterdam"), null until set; the UI sets the browser's
// for a user without one. The time line agents get after a gap is in it, and their agents run with it as `TZ` from
// their next start (#253); without one, both are UTC.
type Settings = {
  model: string | null;
  models: { value: string; name: string }[];
  review: "off" | "required";
  timezone: string | null;
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
  kind: "work" | "review" | "utility"; // "review": a session reviewing a proposal (ADR 24); "utility": an owner's session for the runtime's judgements (ADR 26); `GET /sessions` leaves both out
  review: "off" | "required"; // whether its pull requests are reviewed by another model first; from the user's settings
  created_at: string;    // RFC 3339
  updated_at: string;
  active_at: string;   // when something last happened (an event was recorded); `GET /sessions` is ordered by it, newest first
  deleted_at?: string;   // when it was deleted; `GET /sessions` leaves deleted sessions out
  agent_status?: string; // what the pod is doing while the agent starts, e.g. "cloning https://github.com/…"
  agent_error?: string;  // why the agent last failed to start or exited; it's started again meanwhile. Both clear once it's ready
  parent_id?: string;    // the session that spawned it, for a sub-session (ADR 27)
  limited_until?: string; // the session hit the account's usage limit and carries on by itself then (see "When a turn fails on the account's usage limit" below)
};

// What the session may do (ADR 2). Capabilities so far: "model", "github" (read: public data, plus the "repo"
// grants), "packages" (package registries), "codex" (Codex through the owner's ChatGPT login), "repo"
// {repo, access: "rw", branch, credential?, as?}, "memory" {namespace, access: "rw"}, "docs" (library documentation through Context7's MCP server, which the runtime serves at `/servers/docs` and agents get as `docs` tools; granted by default). `description` says what the grant allows in plain
// terms, in the same words the agent's `list_grants` tool uses.
type Grant = {
  capability: string;
  params: Record<string, unknown>;
  description?: string;
  id?: string;           // for changing or revoking it
  created_at?: string;   // when it was granted
  expires_at?: string;   // when it runs out, if it does
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

`POST /api/v1/sessions` derives the repository and grants before it returns. The owner changes a session's grants while it runs ([ADR 2](adr/0002-capabilities.md)): grant a capability it lacks, change when one runs out, narrow it, or revoke it. A grant that has run out or been revoked stops counting at once, wherever the session's grants are read (the authorizer reads them on every request); egress that depends on grants (direct access to package hosts with `packages`) is applied again, and the agent is told what changed. A minute's sweep removes grants that have run out and tells their agents. Memory can't be granted mid-session, since its key is minted when the pod is placed, and the docs MCP server granted mid-session reaches the agent at its next start. A session gains more repositories later through `POST /api/v1/sessions/{id}/repos`. Given a `credential`, the session acts on that repository with the user's own GitHub token instead of the GitHub App ([ADR 29](adr/0029-grants-name-their-credential.md)): the token has to reach it (one of the repositories it listed when it was added, or any when it listed more than a thousand), and the grant gains `credential` (its id) and `as` (whose it is). The authorizer then attaches the token for git reads and pushes there and for API reads of `/repos/<that repository>/…`; the App's tokens never cover it, a token that's gone is refused rather than replaced by the App, and every request made with it is logged with the session and credential. Pushes are still limited to the session's branches. Only the user does this, and sub-sessions don't inherit it. Repositories also come when the agent requests one and the user approves (`sloppod.repo.grant` proposals). `repo` is the repository the session was started for, which is checked out as its working directory, or else the first one it was given; `branch` is its session branch, the same in every repository. Until the agent names the session, its title is the first line of the first message. It returns `409` when the user has no credential for the agent's model (a Claude account for Claude Code, a ChatGPT login for Codex), and `400` with `{error}` when a given `repo` isn't accessible or `agent` isn't one the runtime has.

### Credentials

Accounts and keys the runtime uses on a user's behalf ([ADR 21](adr/0021-credentials-live-in-the-runtime.md)). Secrets are write-only: the API never returns them. The first credential of a kind becomes the default for it; deleting the default promotes the oldest remaining one. Running sessions use whichever is the default when they make a request. An invalid credential (unknown kind, a private key that doesn't parse, a URL without a host) gets `400` with `{error}`.

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
  | "token";             // secret: a token an own capability's server takes; config: {header?: string} (sent as that header, else as a bearer token)

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

What's left of the usage limits of the user's inference credentials, as last reported (#140). Claude Code reports a Claude subscription's limits in its sessions' `usage_update`s (`_meta["_claude/rateLimit"]`), under the session owner's default Claude credential; they're known once a session has used it. A ChatGPT login's are read from ChatGPT every five minutes, as the Codex CLI reads them, since codex-acp doesn't report them. Each report updates the windows it covers and keeps the others as they were; one that arrives after a newer one changes nothing the newer one said. A ChatGPT report that isn't as expected is logged and ignored. A quota reads as it stands now: a window whose reset has passed shows as unused, and a refusal ends at its `resets_at`. Only credentials with a report are listed.

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

### Proposals

Outward actions and access the agent asked for ([ADR 3](adr/0003-proposals-and-trusted-executor.md), [ADR 2](adr/0002-capabilities.md)). Kinds:

- `github.pr.create`: payload `{repo, base, head, head_sha, title, body, draft, auto_merge}`, proposed with an optional `review` (below); the detail adds `diff` and `commits`, up to `head_sha`, the commit that was proposed. Approving opens the PR only if `head` still points there; otherwise it fails and the agent proposes again. With `auto_merge`, which the agent can ask for and the reviewer decides, sloppod turns on GitHub's auto-merge after opening the PR (with the first merge method the repository allows), so it merges once its required checks pass, and only at the reviewed commit (`expectedHeadOid`); the result's `message` says whether that worked. Opening it records a fingerprint of the change it makes (`fingerprint`: the files and added and removed lines of its diff against `base`, like `git patch-id`; none when binary files change). Pushing to a head with auto-merge on turns it off but lets the push through. Once the push has landed, the PR's diff against its base is fingerprinted again: if it matches, as after a rebase or a merge of the base, auto-merge goes back on, pinned to the new head; if not, including a resolved conflict, it stays off and the PR needs merging by hand. The session is told which. A head without a fingerprint, such as a stack's layer or a PR opened before fingerprints, still has its first push refused with a message, and needs merging by hand. Every notice that a pull request needs merging by hand ends with the next step: the agent can't merge, so it tells the user, with the link.
- `github.stack.create`: pull requests that build on each other, e.g. branches managed with `gh stack`; payload `{repo, base, pull_requests: {head, base, head_sha, title, body, draft}[], auto_merge}`, proposed with an optional `review` like a pull request; bottom first, each based on the one below and the first on `base`. The detail adds `layers: {diff, commits}[]`, one per pull request. Approving checks that every head is still at its `head_sha` before opening anything, opens them (reusing open ones), and links them as a stack on GitHub; the result is `{pull_requests: {url, number}[], stack, message}`, with `stack` null and the reason in `message` if linking failed. With `auto_merge`, which the agent can ask for and the reviewer decides, sloppod turns on auto-merge for the top pull request at its proposed commit once the stack is linked, which merges the whole stack when its required checks pass; without a linked stack it stays off. Until the stack has merged, the runtime checks every 30 s that each layer is still at its proposed commit, and if one has moved, by any route, it turns auto-merge off and tells the session. That narrows, but doesn't close, the window in which a moved layer could merge: its required checks would have to pass between two checks, or while the runtime is down. The watch resumes when the runtime restarts. Edits: `{auto_merge?, pull_requests: {title?, body?}[]}`, the titles and bodies by position.
- **Watching.** Once a pull request from either kind is open, the runtime polls it every minute and tells its session what changed, as a notice: its checks failed (naming them) or pass again after failing, it conflicts with its base, it has new reviews or comments, or it was merged or closed, after which it isn't watched any more. What was last seen of each is kept (`pull_watches`), so a restart neither repeats nor misses a change. A pull request first seen after this was deployed is recorded without a notice. Checks that are running, and mergeability GitHub is still working out, don't count as changes. Auto-merge being turned off or back on isn't reported here: the push that causes it is. Notices name a pull request with its repository (`owner/name#n`), since a session can push to several.
- `sloppod.repo.grant`: write access to a repository; payload `{repo, reason, branch}`; the detail adds nothing.
- `sloppod.repo.act_as_owner`: acting on a repository as the user, with one of their GitHub tokens ([ADR 29](adr/0029-grants-name-their-credential.md)), and write access to the session branch there; payload `{repo, reason, branch, credential, login}`, `credential` the first of the user's tokens that reaches the repository and `login` whose it is. The detail adds `tokens: {id, name, login}[]`, the user's tokens to pick from. Edits: `{credential}`, another token, which has to reach the repository when it's carried out. No standing approval covers it, since approving picks which of the user's identities the session acts as: creating one for this kind is refused (`400`), and its proposals always wait for the user. The result's `login` says whose token it ended up acting with.
- `sloppod.session.report`: a sub-session's report to its parent that its task is done ([ADR 27](adr/0027-sub-sessions.md)); payload `{parent, summary, title, repo?, base?, head?, head_sha?, review?}`, `title` the task's (the session's title unless the child gives one), the branch fields when its branch (the session branch, or the one named after it that the child gives as `head`) has commits on the parent's (or on the default branch, which it started from if the parent hadn't pushed), and `review` (`{verdict: "pass" | "changes" | "stalled", summary?, findings?}`) once its review is over; the detail is the diff, as for a pull request. Between review rounds the child can correct its title and summary with `request_review`'s `update: {title?, summary?}`. It isn't outward: once out of review (or straight away, unreviewed), the runtime approves it (`decided_by: "sloppod"`) and carries it out by telling the parent.
- `sloppod.code_review`: a session's request for a review of a branch, without proposing anything (#254): work in progress, a spike, or a branch it'll split up later. Payload `{repo, base, head, head_sha, focus}`, `head` being the session branch or one named after it, and `focus` what the session asks the reviewer to look at, or `null`; the detail is the diff. It's always reviewed, whatever the session's review policy, so it needs the user's ChatGPT login; the reviewer is told nobody decides from it, so `focus` isn't held to describing the change. Rounds go as for a pull request: findings go to the session, which pushes fixes and calls `request_review` (with `update: {focus?}`) or withdraws it. When the review ends, the session hears how, and the runtime approves it (`decided_by: "sloppod"`) and carries out nothing.
- `github.issue.create`: issues to file in a repository the session can push to; payload `{repo, issues: {title, body, labels}[]}`; the detail adds nothing. Labels that don't exist yet are created. The result lists the issues filed, and any that failed.
- `github.issue.update`: changes to existing issues in a repository the session can push to; payload `{repo, updates: {number, comment?, close?, reopen?, title?, body?, add_labels?, remove_labels?}[]}`, with `close` one of `completed`, `not_planned` or `duplicate`; the detail adds `current: {number, title, body}[]`, each issue whose title or body an update replaces as it is on GitHub now (once, however many updates name it), so a UI can show the change as a diff. Each update posts its comment first, then relabels, then edits and closes or reopens. Labels that don't exist yet are created. The result lists the issues updated, and any that failed.
- `github.pr.update`: changes to pull requests that are already open, in a repository the session can push to, like issue updates; payload `{repo, updates: {number, comment?, close?, reopen?, ready?, title?, body?, add_labels?, remove_labels?}[]}`, with `close`, `reopen` and `ready` each `true` (`false` isn't a change). `close` closes without merging, and `ready` marks a draft ready for review; a pull request can't be merged this way, and an update can't both close and reopen, or close and mark ready. The detail adds `current` as for issue updates, from the pull request's issue. Each update posts its comment first, then relabels, marks a draft ready (not if it no longer is one), then edits and closes or reopens. The result lists the pull requests updated, and any that failed.

A proposal on a repository the session acts on with one of the user's GitHub tokens ([ADR 29](adr/0029-grants-name-their-credential.md)) is read and carried out with that token, in the user's name: its pull requests and issues open as them, and auto-merge is turned on as them. Its payload then has `as`, the token's login, added by the runtime when it's proposed, so a UI can say who it acts as. It's carried out only as that identity: if the session's grant of the repository has switched to another identity since, it fails and the agent proposes again. The runtime's watch of the pull requests it opened, and of a stack's layers, reads them with the same identity.

**Review first** ([ADR 24](adr/0024-automated-review-before-human-review.md)): when the session's `review` is `required`, or the agent proposes a pull request or a stack with `review: true`, and the user has a ChatGPT login, the proposal starts `in_review` rather than `pending`. Without the login, a required review is skipped, and asking for one is an error. The runtime starts a review session (`kind: "review"`, running Codex, with read-only access to the repository and the commit under review checked out, detached, each time its pod starts), which ends each round by calling its `submit_review` tool. A pass moves the proposal to `pending`, where the user sees it and standing approvals can match it. Changes requested move it to `changes_requested` and the findings go to the agent, not the user; the agent pushes fixes and calls `request_review` (with an `update` of the proposal's fields, e.g. a title and body that fit the change again, and replies to findings it disputes), which re-pins the proposal to its branch's new head and runs the next round in the same review session. After three rounds with changes requested, the proposal goes to `pending` anyway, with the open findings in its reviews. Between rounds the review session is parked (ADR 5), and the next round wakes it on the new commit. It's deleted once the proposal passes, reaches that cap or is withdrawn, and when the author's session is deleted its proposals still in review are withdrawn. A sub-session's report is the exception: when its review passes, its reviewer is parked and kept, so a pull request or stack the parent proposes with `from_sub_session` (the sub-session's id) carries that review over (#247). Then the kept reviewer reviews the proposal, told which commit it passed, and looks at what changed since and at the title and description, rather than reviewing the whole change cold. That works when the sub-session's latest executed report passed review, is of the same repository, and its reviewer is still kept; otherwise the review starts afresh, and naming a session that isn't the proposer's sub-session is an error. A kept reviewer no proposal is reviewing is deleted a day after its last proposal was decided. A round that stalls doesn't keep the proposal in review: a reviewer that ends its turn without calling `submit_review` is reminded once, and if it does so again, or the round takes over 30 minutes, the proposal goes to `pending` unreviewed (the author is told, and no standing approval matches it). `GET /api/v1/proposals/{id}/reviews` lists the rounds.

When a proposal is decided or carried out, the runtime tells the agent: the notice is recorded as a `user_message_chunk` with `_meta.sloppod.notice: true`, and, for one a UI should show folded, a one-line `_meta.sloppod.summary` (messages and reports from other sessions; the agent gets the full text) and delivered as a new turn when the agent is idle, steered into a running turn once the agent has no tool call in flight and no permission request open (steering cancels either, [ADR 4](adr/0004-asynchronous-interaction.md)), or queued until the agent is up. Either way it's kept with the session's queue until the agent answers the request that carries it (the turn's, or the steer's). After a runtime restart, the supervisor reports the requests the agent still has open: what was kept and is among them isn't sent again, and the rest is. It can come twice only if a restart falls between sending it and noting what it was sent as. What the agent gets of a notice leads with a line giving the time in the owner's timezone, e.g. `[sloppod: it's 2026-10-27 16:40 CET]`; the recorded notice doesn't have it. Sessions whose turn a runtime restart cut off get a notice of the same kind once their agent is back, naming the build now running (`SLOPPOD_BUILD`, the chart's app version).

When a turn fails on the account's usage limit (claude-agent-acp's `errorKind: "rate_limit"` or its "You've hit your …" text, codex-acp's `codexErrorInfo: "usageLimitExceeded"`), the session waits for the limit to reset rather than counting it as an ordinary failed turn. `limited_until` on the session says until when: just after the reset, if the quota of the credential the session uses (its owner's default for the agent) says it's limited until then, otherwise 30 minutes on. When that quota changes, the wait follows: to a newly known reset, or to now once the credential isn't limited any more. Meanwhile nothing starts a turn by itself: messages queue as they do behind a turn (shown in `_sloppod/queue`), also while the agent isn't up, without waking a parked session; notices queue too, and an agent that comes back keeps the queue waiting. Steering a queued message in (`_sloppod/steer_queued`) tries it now; while the agent isn't up, that ends the wait, carrying on with everything queued. At `limited_until`, the agent gets a notice to carry on where it left off, in one turn with everything queued. A turn that hits the limit again waits again, and one that ends well ends the wait. The wait survives a runtime restart.

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

Approving returns as soon as the decision is recorded, with status `approved`; the proposal is carried out in the background and its outcome (`executed` or `failed`) follows as a `_sloppod/proposal` update, and in `GET /api/v1/proposals/{id}`. Once carried out, its outcome is recorded even if the database is briefly away: the write is tried again, waiting up to a minute between tries, rather than the proposal being left `approved` and carried out again later. A proposal still `approved` when the runtime restarts is carried out again on startup; executors don't repeat work already done: an open pull request from the same head into the same base is reused, and the issues and comments a proposal posts carry a hidden marker (`<!-- sloppod:proposal/<id>/<n> -->`, the proposal and the item's position in it), by which they are found and not posted again, however long ago and whatever else says the same. Approving with edits changes the payload first: `github.pr.create` takes `{title?, body?, auto_merge?}`; `github.stack.create` takes `{pull_requests?: {title?, body?}[], auto_merge?}`, the pull requests' edits by position; `github.issue.create` takes `{issues}`, the issues to file, so leaving one out drops it; `github.issue.update` and `github.pr.update` take `{updates}` the same way. `sloppod.repo.grant` takes no edits; `sloppod.repo.act_as_owner` takes `{credential}`. A proposal's status only moves forward (`in_review` and `changes_requested`, back and forth between review rounds; then `pending`; then `approved` or `rejected`; then `executed` or `failed`), so a UI can ignore a copy, e.g. from a slow refetch, that is behind the one it has. Undecided proposals, in review or not, can be withdrawn.

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

A WebSocket carrying [ACP](https://agentclientprotocol.com) JSON-RPC 2.0, one message per text frame. The runtime is the agent from the UI's point of view. ACP `sessionId`s are runtime session IDs. Sessions are created through REST, not `session/new`. Like any write through the REST API, a browser can only open it from sloppod's own pages: a request whose `Origin` is neither the scheme and host it was made to (`X-Forwarded-Proto` and `X-Forwarded-Host`, or `http` and `Host`) nor one of `SLOPPOD_ALLOWED_ORIGINS` gets 403. Clients that send no `Origin`, like `sloppodctl`, are unaffected.

UI → runtime:

| Method | Notes |
|---|---|
| `initialize` | Returns the agent's capabilities, including `_meta.steering`. |
| `session/load` `{sessionId, cwd: "/", mcpServers: []}` | Replays the latest of the session's history (up to 2000 updates) as `session/update` notifications, then responds. When there's older history, the response's `_meta.sloppod.historyBefore` says where it ends; ask for it with `_sloppod/history`. From then on the connection receives that session's live updates. Several sessions can be loaded on one connection. |
| `_sloppod/history` `{sessionId, before}` | The page of history before `before` (up to 2000 updates), oldest first: `{updates: SessionUpdate[], before}`, where `before` is where to ask for the page before it, or `null` once there's no more. For a loaded session. |
| `session/prompt` `{sessionId, prompt: ContentBlock[]}` | Starts a turn. The prompt can carry `image` blocks (base64 `data` and a `mimeType`) when the agent says it takes them (`initialize`'s `agentCapabilities.promptCapabilities.image`); the runtime passes them through as they are. The web UI scales an image down to 1568 px on its longest side, re-encodes one over 3.5 MB (the model API's 5 MB limit once base64-encoded), and sends at most 10 per message. Responds `{stopReason}` when the turn ends. While the agent isn't up (`starting`, `disconnected`) the prompt is queued: it's echoed right away, everything queued runs as one turn once the agent is ready, and each queued request responds when that turn ends. While a turn is running, the prompt waits behind it instead: it isn't echoed yet, it shows in `_sloppod/queue`, and everything waiting runs as the next turn when this one ends (echoed then). A prompt or steer that reaches the agent 30 minutes or more after it was last heard from, or on a later date (in the owner's timezone), leads with a line for the agent only, not echoed or recorded: `[sloppod: it's 2026-10-27 16:40 CET, 3 hours since the last message]`, in the owner's timezone. After a runtime restart, the gap counts from the session's `active_at`. Queued prompts and notices survive a runtime restart: they're kept until the agent answers the turn that takes them (or, after a restart, still has it open), then run once the agent is back (or wake a parked session); a prompt request pending across a restart gets no response, since its connection is gone, but its turn shows in the transcript. The same holds when the agent's link drops before the agent answers a turn: what the turn carries, including a prompt that started it directly, stays kept, and the next link says whether the agent has it; if not, it runs again, logged once. Its pending `session/prompt` requests respond with an error saying it's kept. With `_meta.sloppod.messageId` (a UUID), the message is echoed, and queued, under that id, so a UI whose connection dropped before the response can tell from the next `session/load` whether the message arrived, and send it again if not. A prompt whose `messageId` the session already has, recorded or waiting in the queue, doesn't run again: it answers `{stopReason: "end_turn", _meta: {sloppod: {alreadyReceived: true}}}` right away. A UI can't always tell from what it loaded, since it only has the latest of a long history. `_session/steering` takes the same. |
| `session/cancel` `{sessionId}` (notification) | Cancels the running turn. The runtime answers the turn's open permission requests and questions as cancelled (the client needn't, unlike plain ACP) and sends `_sloppod/permission_resolved` for them; `ask_user` questions stay open. Messages still queued for an agent that isn't up stay queued. |
| `_sloppod/steer_queued` `{sessionId, id}` | Moves a message waiting behind the running turn into it, as a steer (echoed with `_meta.sloppod.steering`), and responds once the agent has it; its `session/prompt` request then responds too. Until then the message is kept with the session's queue, though no longer shown in `_sloppod/queue`, and not echoed; a `session/prompt` sent again with its `messageId` meanwhile answers `alreadyReceived`. If the steer can't be delivered (the agent isn't up, its link drops, the agent errs), it errors, and the message goes back to its place in `_sloppod/queue`, its `session/prompt` request still waiting; it runs at once if the agent is idle by then. A runtime restart in between leaves it kept: delivered if the agent still has the steer open and takes it, otherwise back at the front of the queue. |
| `_sloppod/remove_queued` `{sessionId, id}` | Takes a message waiting behind the running turn out of the queue without running it, e.g. so the user can edit it, and responds `{prompt: ContentBlock[]}` with what it held. Its `session/prompt` request responds `{stopReason: "cancelled"}`, as does a `session/prompt` sent again with its `messageId` by a UI that lost its connection before hearing so, even across a runtime restart. Like `_sloppod/steer_queued`, it errors if the message isn't queued any more. |
| `_session/steering` `{sessionId, prompt: ContentBlock[]}` | Delivers a message into the running turn. Before the agent is up it joins the queue like a prompt and returns `{outcome: "queued"}`. Otherwise it responds once the agent has it, and is echoed (with `_meta.sloppod.steering`) then; until then it's kept with the session's queue, so a runtime restart in between delivers it rather than losing it. If it can't be delivered it errors and isn't echoed or kept, for the client to send again. |
| `session/set_config_option` `{sessionId, configId, value}` | Changes one of the session's config options from the load response's `configOptions`, e.g. `model`. Responds `{configOptions}`. The session keeps its model across restarts. |

Sessions always run in their agent's most autonomous mode, Claude Code's auto mode or Codex's full access: the sandbox and the session's grants bound what any tool call can reach. Mode isn't configurable.

Runtime → UI:

| Method | Notes |
|---|---|
| `session/update` | Standard ACP updates: `user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`, `available_commands_update`, `config_option_update`, compaction reports, and `notice` (`{severity, title, description}`, e.g. why the agent failed to start; recorded once per distinct failure). Prompts sent by any UI are echoed as `user_message_chunk` with a `messageId` per prompt, so every viewer sees them; steers carry `_meta.sloppod.steering: true`, and notices to the agent (see Proposals) `_meta.sloppod.notice: true`. |
| `session/request_permission` (request) | Sent to every UI that has the session loaded, including requests still open when a UI loads the session (after the `session/load` response). The first answer wins. |
| `elicitation/create` (request) | The agent asks the user something, e.g. with `ask_user` (below). Delivered like a permission request. Params: `{sessionId, mode: "form", message, requestedSchema, toolCallId?}`; the schema is an object of primitive fields (below). Answer `{action: "accept", content}`, `{action: "decline"}` (skip: the agent is told the user didn't answer) or `{action: "cancel"}`. |
| `_sloppod/queue` `{sessionId, messages: {id, prompt}[]}` | The messages waiting behind the running turn, sent whenever that changes and when a UI loads the session. |
| `_sloppod/permission_resolved` `{sessionId, toolCallId, requestId}` | Sent to every UI, including the one that answered, once a permission request or question is answered or cancelled. `requestId` is the JSON-RPC id the request had on this connection. |
| `_sloppod/session` `{session: Session}` | The session's phase, title, repository or grants changed. |
| `_sloppod/proposal` `{proposal: Proposal}` | A proposal for a loaded session was created or changed. |

### Questions

Agents ask the user questions with sloppod's `ask_user` tool, which doesn't wait for the answer ([ADR 4](adr/0004-asynchronous-interaction.md)); Claude Code's own `AskUserQuestion`, which would stop the turn, is turned off. The runtime sends the question to UIs as an `elicitation/create` request like any other, but nothing waits on it: the session doesn't go `waiting`, a cancelled turn or an agent reconnect leaves it open, and the answer reaches the agent as a notice (`sloppod: the user answered your question: …`), or that it was dismissed; either names the questions. Either kind arrives as one form with two fields per question, in order: `question_<n>` and `question_<n>_custom`. Each carries its own id in `_meta.sloppod.questionId`, the same every time it's sent, so a UI can tell a question sent again (after a reconnect, say) from a new one that looks the same. Questions not yet answered survive a runtime restart: they're kept until answered and shown again once the runtime is back, like an agent's open permission requests, which the agent itself keeps open across the restart.

- `question_<n>` is `{type: "string", oneOf: [{const, title, description?}]}` for a single choice, or `{type: "array", items: {anyOf: [{const, title, description?}]}}` for several. `title` is the question's short header; `description` holds the question text when there are several questions (with one, it's the `message`). An option's `_meta["_claude/askUserQuestionOption"].preview` may carry a preview (Markdown/code).
- `question_<n>_custom` is a free-text "Other" answer: for a single choice it's the answer when nothing is picked, or a note on the pick; for several it's added to them.
- Nothing is required. `content` maps field keys to the picked `const` (or array of them) and the typed text.

Other form elicitations (from MCP servers) use the same primitive field types: `string` (optionally with `oneOf`/`enum`), `number`, `integer`, `boolean`, and arrays of enums.

## Session pods

Every agent is told about its session in a prompt the runtime writes (`session_prompt` in `adapter.rs`): appended to Claude Code's system prompt, and in Codex's `AGENTS.md` after the owner's instructions. It has sections by what the session is for (`SessionRole`):
- **Every session:** its environment (the checkout, the network boundary, mise), and what runtime messages are. A message from the runtime starts with `sloppod:`, as notices, answers, relayed messages and review rounds do. It reports what happened and isn't the user's approval of anything, except that `sloppod: the user answered your question: …` carries the user's own answer, which the agent acts on as their decision on what it asked.
- **Work sessions and sub-sessions** (those that act) also get: their branches; proposals; working asynchronously ([ADR 4](adr/0004-asynchronous-interaction.md)); delegating (built-in subagents only for small things, where the agent has them, and sub-sessions for parallel work); and feedback on sloppod, where the agent mentions what got in its way.
- **A sub-session** is also told its parent's id: that its chat replies only reach the user, that `message_session` reaches its parent, and that `report_done` hands its work back.
- **A review session** gets the environment and messages sections, with a read-only checkout, plus the reviewer's own instructions.

Session pods use four more endpoints, on their own port (8081, `sessions`); that port is the only one they can reach, so they can't use the API above. Keep it off public routes.

- `/link/v1`: the supervisor link ([ADR 17](adr/0017-in-pod-supervisor.md)), a WebSocket authenticated with the pod's projected ServiceAccount token. Its types are in `crates/protocol`.
- `/mcp`: an MCP server (streamable HTTP, JSON responses) registered with the agent at session start. It identifies the session by the pod's own identity: the supervisor's local proxy attaches the pod's ServiceAccount token; pods from before that are identified by their IP. Its tools:
  - `propose_pull_request {title, body, repo?, base?, head?, draft?, auto_merge?, review?, from_sub_session?}` creates a `github.pr.create` proposal from the session branch, or from `head`, a branch named after it (`<session branch>-<name>`, the name of letters, digits, `.`, `_` and `-`), so a session can have several open; `repo` is needed when the session can write to more than one.
  - `propose_issues {issues: {title, body?, labels?}[], repo?}` creates a `github.issue.create` proposal (up to 50 issues; `repo` as for pull requests).
  - `propose_issue_updates {updates: {number, comment?, close?, reopen?, title?, body?, add_labels?, remove_labels?}[], repo?}` creates a `github.issue.update` proposal (up to 50 updates), e.g. to close issues the session's work finished, or merge duplicates. `close` is `completed`, `not_planned` or `duplicate`; an update with any other field, or one of the wrong type, is refused.
  - `propose_pull_request_updates {updates: {number, comment?, close?, reopen?, ready?, title?, body?, add_labels?, remove_labels?}[], repo?}` creates a `github.pr.update` proposal (up to 50 updates), e.g. to close a draft another pull request replaced, or mark a draft ready; `close`, `reopen` and `ready` are `true`. Merging isn't among them.
  - `request_repository {repo, reason}` creates a `sloppod.repo.grant` proposal; a sub-session can't, as it works only where its parent can push, and is told to say in its report that it needs the repository. A refusal for a repository the session can't push to, and `list_grants`, point to this tool only where the session can use it.
  - `request_owner_identity {repo, reason}` creates a `sloppod.repo.act_as_owner` proposal, to act on a repository with one of the user's GitHub tokens ([ADR 29](adr/0029-grants-name-their-credential.md)); refused for a sub-session, when the user has no token, when none reaches the repository, or when the session already acts as the user there.
  - `get_proposal {id}` returns a proposal's status and result. The agent is also told when its proposals are decided.
  - `withdraw_proposal {id, reason?}` withdraws one of the session's pending proposals: it becomes `withdrawn` (with the reason) and nothing is carried out.
  - `spawn_session {task, title?, repo?, grants?, agent?, model?}` starts a sub-session ([ADR 27](adr/0027-sub-sessions.md)) with `task` as its first message, and answers in a sentence with its id, title and branch. It has the session's grants, or only the capabilities named in `grants` (model access always stays), and the session's owner, review policy and, unless given, agent and model. It works in `repo`, one the session can push to (by default the session's), on `<session branch>-<first 8 of its id>`, which starts from the session branch as pushed (from the default branch if it isn't pushed yet). A tree of sessions has at most 5 sub-sessions running at once (the tool's description says the same, from the same constant). Work sessions only.
  - `list_sessions` lists the session's sub-sessions: `{id, title, phase, repo, branch}` each.
  - `message_session {id, text}` sends a message to the session's parent or one of its sub-sessions, delivered as a notice; a parked sub-session is woken for it unless its tree is at the limit, in which case it's refused.
  - `report_done {summary, title?, head?, review?}`, for a sub-session, creates a `sloppod.session.report` proposal: reviewed like a pull request under the session's review policy, then delivered to the parent with the summary, the branch and how the review ended. `head` names the branch the work is on when it isn't the session branch, e.g. a reused child's later task on `<session branch>-<name>` (the same rule as `propose_pull_request`'s `head`); a named branch without commits on the base is refused. `title` says what the task was, for a child reused for more than one.
  - `request_code_review {head?, base?, repo?, focus?}` creates a `sloppod.code_review`: a review of a pushed branch (the session branch by default) against `base` (the default branch by default), with nothing proposed. Its success text says so: no human is asked and nothing is carried out. `request_review` takes it for the next round, with `update` limited to `{focus}`.
- `/memini/*`: memini, for sessions with a `memory` grant, identified like `/mcp`. The runtime forwards each request to the owner's memini with the session's own key and namespace, and refuses memini's administration endpoints. The memini plugin's `MEMINI_BASE_URL` points here.
- `/servers/<name>` and below: the MCP server of a capability the user defined, or of a built-in that's one (`docs`, Context7's), for sessions granted it, identified like `/mcp`. The runtime forwards each request to the definition's `mcp_url`, or the built-in's (the bare path to the URL itself, paths below it appended), with the definition's `token` credential attached (a built-in's gets none) and the agent's own `authorization` and `cookie` headers dropped, and streams the answer back. It never goes back to the runtime itself, by its pod or Service address, or to a loopback, unspecified or link-local one: names are resolved for each connection and those addresses left out, so `502` if that leaves none. Redirects come back to the agent rather than being followed. A session without the grant gets `403`; a definition deleted, or deleted and made again under its name since it was granted, gets `404` or `403`. Each request is logged with the session and the server's name. The agent's MCP config lists each granted one under its name.

Neither `/memini/*` nor `/servers/<name>` passes on headers that say who or where a request is from: `Forwarded`, `X-Real-IP`, any `X-Forwarded-*`, and the user header (`SLOPPOD_USER_HEADER`). The request comes from the runtime, so the agent could otherwise name any user to a service that trusts them.

A session that's stopped, parked or deleted can't act, even if its pod is still up: the authorizer refuses its egress, `/mcp` gives it no tools, and `/memini/*` answers 403.

In the pod, the repository is checked out at `/workspace/repo` on the session branch. `/workspace` is the session's own volume, kept across pod restarts.
