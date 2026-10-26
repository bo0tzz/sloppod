# 7. Extension points have plugin-shaped boundaries

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Much of sloppod is meant to vary:

- which agents it runs
- which services sessions can reach, and how requests to them are authorized
- which outward actions exist
- what can wake a session
- where sessions are placed
- where memory lives

We'll start with built-in implementations, but should be able to extract them into real plugins later without rewriting the core.

## Decision Outcome

Every extension point is a narrow, versioned contract, and the core only talks to implementations through it.

- **Contracts:** inputs and outputs are plain, serializable data. No shared mutable state, and no reaching into core internals.
- **No special-casing:** built-in implementations register through the same interface an external plugin would use. The core never branches on which implementation it is dealing with.
- **Declarative manifests:** each implementation describes itself with a manifest (name, version, what it contributes, configuration schema), which the runtime stores like any other config ([ADR 6](0006-self-reference.md)).
- **In-process for now:** in Rust ([ADR 14](0014-rust.md)), contracts are traits whose arguments and results are serde types, shaped so the implementation can later move behind a process boundary (gRPC or JSON-RPC, in the style of MCP and ACP) without changing callers.
- **Plugins bundle contributions.** A plugin can contribute to several extension points. For example, a GitHub plugin would provide:
  - a credential provider (GitHub App installation tokens)
  - a request policy (REST, GraphQL and git smart HTTP)
  - proposal kinds (`github.pr.create`, `github.comment`)
  - a wake source (webhooks)

Extension points so far:

| Extension point | Contract |
|---|---|
| Agent adapter | Starts an agent in a session pod and speaks ACP ([ADR 9](0009-acp.md)) |
| Credential provider | Mints a credential for a grant, with expiry |
| Request policy | Decides one request for a service and says which credential to attach ([ADR 11](0011-enforcement-at-the-pod-boundary.md)) |
| Capability type | Bundles tools, egress, credential provider, request policy, MCP servers and instructions ([ADR 2](0002-capabilities.md)) |
| Proposal kind | Schema, renderer, executor ([ADR 3](0003-proposals-and-trusted-executor.md)) |
| Enforcer | Applies a session's egress and redirection to the data plane, e.g. Cilium |
| Wake source | Emits events that can satisfy wake conditions ([ADR 5](0005-park-and-wake.md)) |
| Placement backend | Creates, suspends and deletes session pods ([ADR 16](0016-agent-sandbox.md)) |
| Memory provider | Memory storage and recall ([ADR 8](0008-shared-memory-service.md)) |

### Consequences

- Good: extracting plugins later is mostly packaging, and third parties can extend sloppod without forking it.
- Bad: we design interfaces before we've learned their shape. Versioned contracts will need care when we change them.
