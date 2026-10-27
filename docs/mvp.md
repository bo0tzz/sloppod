# MVP: dogfooding

The first milestone is moving sloppod's own development into sloppod. The MVP is whatever that takes, and nothing more: every later feature gets built from inside sloppod.

## Done when

A sloppod session can take a change to this repository from prompt to reviewed PR:

1. Start a session from the UI by sending a first message that mentions `bo0tzz/sloppod`.
2. Chat with the agent while it works, including sending messages mid-turn.
3. The agent edits, builds and tests in its pod, using tools from the repository's `mise.toml`.
4. The agent pushes to a session branch and proposes a PR. The PR is opened only after approval in the UI.
5. The session survives its pod being deleted, and can be resumed later from the UI.

## In scope

| Area | MVP version | Later |
|---|---|---|
| Sessions ([1](adr/0001-sessions-decoupled-from-directories.md)) | Runtime-owned session object, event log and transcript blob. Resume after pod loss. A session starts from its first message, which decides the repository it's about (if any). Messages sent before the agent is up are queued. Decided proposals and repositories added later are delivered to the agent as notices | Projects, sub-sessions, messaging |
| Capabilities ([2](adr/0002-capabilities.md)) | Grants stored per session, from a fixed set: `model`, `github` (read), `packages`, `repo` (`rw`, session branch only) when the first message names one, `memory` when memini is configured. More `repo` grants come from the agent's `request_repository` proposals or the session menu | Requests for other capabilities, approvals with TTL, standing approvals, profiles, starting-set suggestion |
| Proposals ([3](adr/0003-proposals-and-trusted-executor.md)) | One kind, `github.pr.create`: review title, body and diff; approve or reject | Editing fields, more kinds, a reviewer model |
| Async ([4](adr/0004-asynchronous-interaction.md)) | Prompts and steering through the adapter; proposals return a pending ID | Inbox urgency levels, delivery of grant changes |
| Park/wake ([5](adr/0005-park-and-wake.md)) | Not in the MVP | Everything |
| Self-reference ([6](adr/0006-self-reference.md)) | Not in the MVP | Everything |
| Memory ([8](adr/0008-shared-memory-service.md)) | memini through its Claude Code plugin (loaded via the adapter's SDK `plugins` option). The runtime decides the namespace (the repository's `owner/repo`, or `<home>/unsorted` without one), passes it as `MEMINI_NAMESPACE`, and the authorizer attaches the key, pins the namespace header and refuses memini's administration endpoints. Claude Code's built-in auto memory is off. Your user-level instructions are rendered into every session, and the repository carries its own `CLAUDE.md` | Namespaces for sessions with several projects, limits on memini's escape hatches, project links |
| ACP ([9](adr/0009-acp.md)) | Runtime as ACP client to `claude-agent-acp`, and `/acp` over WebSocket for the UI | Other agents, several UIs per session |
| Pods ([10](adr/0010-sessions-run-in-pods.md), [16](adr/0016-agent-sandbox.md), [17](adr/0017-in-pod-supervisor.md)) | One Sandbox per session, fixed resources, supervisor connects outward; workdir PVC per session, transcript synced on shutdown | Warm pools, resize, suspend for parking, `exec@<host>`, agent shutdown handlers |
| Enforcement ([11](adr/0011-enforcement-at-the-pod-boundary.md)) | Egress allowlist plus redirection to the Envoy listener; an authorizer with request policies for the model API, memini and GitHub (reads, session-branch pushes, writes refused with a pointer to the propose tool) | Request policies for more services, grant-driven changes, short-lived SSH certs |
| Tools ([12](adr/0012-tooling-via-mise.md)) | `mise install` from the repository's locked `mise.toml` at session start, into the session's own volume | Trusted installer, read-only store, base dev tool set, tool requests from the agent, dependency caching |
| Repositories | Fresh clone into the session's volume, through the authorizer | Runtime-managed mirrors that sessions clone from, work-in-progress snapshots |
| Containers ([13](adr/0013-container-workloads.md)) | Not in the MVP | Everything |
| UI ([18](adr/0018-ui.md)) | Session list, chat (streaming, tool calls, permission requests, steering), PR and repository-access proposal review, adding a repository to a session, the model per session and a default in settings | Board, approval queue, grants, config |
| Agent permissions ([11](adr/0011-enforcement-at-the-pod-boundary.md)) | Sessions always run in Claude Code's auto mode; its permission prompts are judgment calls, while the sandbox and grants are the boundary. Users pick a default model in settings and switch a session's model in the session | Per-project defaults |
| Credentials ([21](adr/0021-credentials-live-in-the-runtime.md)) | Claude accounts, GitHub Apps and memini in the database, encrypted, added in the UI; the owner's default is used | Choosing per grant, switching under a running session |
| Users ([23](adr/0023-users-and-ownership.md)) | A user per request from the proxy's header; owners on sessions and credentials; users see only their own | Sharing, teams, roles |
| Storage ([15](adr/0015-postgres.md)) | Postgres with migrations | — |

## Milestones

1. **Skeleton:** runtime binary, Postgres schema, the session image with supervisor and adapter, and the egress listener plus authorizer with the model-API request policy. A CLI test client creates a session and gets a reply from a prompt.
2. **Chat:** the UI connects over `/acp`, with streaming, permission requests, steering, and resume after the pod is deleted.
3. **Repository work:** checkout of the session branch, memini, mise tools, the GitHub request policy, pushing to the session branch, and the `github.pr.create` proposal with a review screen and an executor.
4. **Switch:** develop sloppod in sloppod.
