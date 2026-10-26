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
- **Profiles** are named default sets of capabilities.
- **Starting set:** a new session gets a suggested starting set (capabilities, resources, model) from a cheap model reading the first message. The human confirms; anything under an `auto` policy just applies.
- **Revocation:** a grant can be revoked at any time. Revocation takes effect at the enforcement layer ([ADR 11](0011-enforcement-at-the-pod-boundary.md)), not by asking the agent.

### Consequences

- Good: sessions run with least privilege by default, and widening it is explicit, audited and time-limited.
- Good: the same model covers repositories, clusters, hosts, tools and pod resources.
- Bad: the runtime has to implement a policy engine and an approval UX. A badly designed request flow will just train the human to click "approve".

## More Information

Prior art:

- [Teleport access requests](https://goteleport.com/docs/identity-governance/access-requests/access-request-configuration.md)
- [Vault leases](https://developer.hashicorp.com/vault/docs/concepts/lease)
- [AWS session policies](https://docs.aws.amazon.com/IAM/latest/UserGuide/access_policies.html#policies_session)
- [Sandstorm Powerbox](https://docs.sandstorm.io/en/latest/developing/powerbox/)
- [CB4A draft](https://www.ietf.org/archive/id/draft-hartman-credential-broker-4-agents-00.html)
