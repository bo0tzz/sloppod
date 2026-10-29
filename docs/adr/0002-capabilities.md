# 2. Capabilities are composable, requestable grants

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Today an agent session inherits everything its host user can do: every kubeconfig, SSH key, GitHub token and cloud credential. Permission rules and auto-mode classifiers only judge what the agent says it will do; they don't limit what it can do. A denied `kubectl` still leaves `curl` with the token from `~/.kube/config`.

We want each session to hold only what it needs, to be able to ask for more mid-session, and to give it up again.

## Decision Outcome

Access is modeled as **capabilities**. A capability is a declarative bundle that can contain:

- tools (a mise fragment, see [ADR 12](0012-tooling-via-mise.md))
- a credential provider (e.g. Kubernetes TokenRequest, a GitHub App installation token, an SSH certificate from the runtime's CA, an exec helper)
- a request policy, which decides individual requests to a service ([ADR 11](0011-enforcement-at-the-pod-boundary.md))
- network reach (egress rules)
- MCP servers
- an instruction snippet for the agent
- a grant policy: `ambient`, `auto`, `ask` or `never`, with a maximum TTL

Examples:

- `repo:<remote>:ro|rw`
- `kube:<cluster>:ro`
- `metrics:<cluster>`
- `exec@<host>`
- `namespace` (a per-session namespace for extra workloads)
- `resources` (CPU and memory for the session pod)
- `model:<provider>` (inference access for the session's agent)
- `web:read` (`GET`/`HEAD` to the open web with no credentials attached), plus narrower variants such as docs sites or package registries

**Default deny:** a session with no grants can reach nothing except its own channel to the runtime. Everything else is a capability, including model access and read-only web access.

A **grant** is a capability given to a session for a duration.

### Rules

- **Composition:** within a session, grants add up: tools, egress and credentials are combined.
- **Attenuation:** delegation only narrows. A sub-session or spawned worker *starts with* at most a subset of its parent's grants, so a parent can never hand out more than it holds. This follows AWS session policies, Devin's profiles and Paseo's sessions. Beyond that, a sub-session requests grants of its own like any other session.
- **Requests:** the agent can request a capability mid-session. A request carries:
  - a capability type
  - optionally one or more suggested targets
  - the access level (`ro`/`rw`)
  - a duration
  - a reason

  The human approves as-is, narrows it, swaps the target, or rejects it. When the agent suggests no target, the human picks one. The reason is recorded for audit but doesn't affect the decision. Requests have a pending TTL separate from the grant's maximum duration, and support delayed start. A session can never approve a request. This follows Teleport access requests and Sandstorm's Powerbox.
- **Only humans approve.** There is no session-facing surface for approving anything. Approvals come from a human, or from a standing approval or policy a human configured.
- **Standing approvals:** a human can pre-approve a request pattern (capability, target, which sessions, TTL) so matching requests are granted automatically.
- **Profiles** are named default sets of capabilities. A project can name one as its default ([ADR 1](0001-sessions-decoupled-from-directories.md)).
- **Starting set:** a new session gets a suggested starting set (project, capabilities, resources, model) from a cheap model reading the first message. The human confirms; anything under an `auto` policy just applies.
- **Revocation:** a grant can be revoked at any time. Revocation takes effect at the enforcement layer ([ADR 11](0011-enforcement-at-the-pod-boundary.md)), not by asking the agent.

### Consequences

- Good: sessions run with least privilege by default, and widening it is explicit, audited and time-limited.
- Good: the same model covers repositories, clusters, hosts, tools and pod resources.
- Bad: the runtime has to implement a policy engine and an approval UX. A badly designed request flow will just train the human to click "approve".

## More Information

MVP, 2026-10-27: the first request is the agent's `request_repository` tool: `repo` with `rw` on the session branch, one suggested target and a reason. It is carried as a `sloppod.repo.grant` proposal ([ADR 3](0003-proposals-and-trusted-executor.md)), so it shares the review queue and executor; the human approves or rejects it as-is. Durations, pending TTLs, narrowing and swapping the target aren't built. The user can also add a repository from the session directly. Either way the agent is told.

Prior art:

- [Teleport access requests](https://goteleport.com/docs/identity-governance/access-requests/access-request-configuration.md)
- [Vault leases](https://developer.hashicorp.com/vault/docs/concepts/lease)
- [AWS session policies](https://docs.aws.amazon.com/IAM/latest/UserGuide/access_policies.html#policies_session)
- [Sandstorm Powerbox](https://docs.sandstorm.io/en/latest/developing/powerbox/)
- [CB4A draft](https://www.ietf.org/archive/id/draft-hartman-credential-broker-4-agents-00.html)

2026-10-27: the `github` capability is read access to public GitHub. Each `repo` grant is what lets the session's GitHub credential reach a repository: reads at any access level, and pushes with `rw`. Tokens are minted for exactly those grants ([ADR 11](0011-enforcement-at-the-pod-boundary.md)).

2026-10-28: a `docs` capability ([#195](https://github.com/bo0tzz/sloppod/issues/195)), granted to new sessions by default: agents get Context7's hosted MCP server (`mcp.context7.com`) as `docs` tools, to look up current library documentation by library and topic. Its request policy lets through only the MCP endpoint, so no other part of the web opens up, and the exfiltration surface is that one service's query API. It's used without an API key, so it's rate limited; a key from a credential can come later. A session without the grant gets neither the server nor the egress.

2026-10-28: a standing approval belongs to one session: it covers that session's matching requests and proposals, and no others, so approving a pattern for one piece of work never widens what other sessions get. That's how they were built (`docs/api.md`, Standing approvals); "which sessions" above means that one session.

2026-10-28: grants can run out and the owner controls them while the session runs (#70, #112, for [ADR 28](0028-acting-as-the-owner.md)). A grant has an optional expiry; past it, or once revoked, it stops counting at once, since every reader of a session's grants (the authorizer on every request among them) leaves it out. The owner grants a capability the session lacks, extends or clears an expiry, narrows a grant (a repository it can push to becomes read only) or revokes it, through the session's grants list; the agent is told each change, and egress that depends on grants is applied again. Grant policies, pending TTLs and delayed starts are still to come.

2026-10-28: the starting set's first part (#232). A judgement suggests the title, the repository and which capabilities (from `github`, `packages`, `codex`) the first message needs; `docs`, read-only and reaching one service, comes with every session, and the user edits them before sending. A session starts with only what's sent, which can only narrow the defaults. Projects, resources and profiles are still to come.

2026-10-29: the bundle above is meant literally. A capability is a definition, data naming its parts, and the runtime assembles a session's reach from its grants' definitions without branching on names. What's built so far is a fixed set of names wired by hand (`repo`, `memory`, `docs`, `packages`, `model`, `codex`), so adding one means changing sloppod. Two decisions make the bundle real:
- **Owners define their own capabilities** in Settings and the API, as they do credentials. The plainest is an MCP server: a URL, the egress to reach it, and the credential to attach outside the pod ([ADR 29](0029-grants-name-their-credential.md)). Sloppod passes it through without knowing what it serves, e.g. the owner's VictoriaLogs and VictoriaMetrics servers, granted with `ask` to the sessions that need them. Request policies are built-in ones by name (`read-only`, `all`) or an extension's.
- **The built-ins become definitions shipped with the runtime,** keeping their names. What no definition can express comes from an extension the definition names ([ADR 7](0007-extension-points.md)): git push rules and proposals for `repo`, memini keys for `memory`, and scoped GitHub tokens for `github`.

Still to decide: per-owner or deployment-wide definitions, MCP servers through the authorizer or proxied by the runtime, and how params narrow a user-defined capability.
