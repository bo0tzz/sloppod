# sloppod

A runtime for coding-agent sessions. Sessions run in sandbox pods on Kubernetes, are not tied to any directory, and get access to things through composable, requestable capabilities. Outward-facing actions go through human review unless a standing approval covers them.

## Docs

- [MVP: dogfooding](docs/mvp.md)
- [Deploying](docs/deploy.md)
- [Runtime API](docs/api.md)

### Architecture decisions

ADR numbers are permanent. This index groups them by topic, concepts first. ADRs record decisions, not what's built: [docs/mvp.md](docs/mvp.md) says which parts the MVP covers.

Concepts:

- [1. Sessions are decoupled from directories and owned by the runtime](docs/adr/0001-sessions-decoupled-from-directories.md)
- [2. Capabilities are composable, requestable grants](docs/adr/0002-capabilities.md)
- [3. Outward actions are proposals executed by the runtime](docs/adr/0003-proposals-and-trusted-executor.md)
- [4. Interaction is asynchronous](docs/adr/0004-asynchronous-interaction.md)
- [5. Sessions can be parked and woken](docs/adr/0005-park-and-wake.md)
- [25. Sessions move through their lifecycle by themselves](docs/adr/0025-session-lifecycle.md)
- [6. The runtime is self-referential](docs/adr/0006-self-reference.md)
- [7. Extension points have plugin-shaped boundaries](docs/adr/0007-extension-points.md)
- [23. Users and ownership](docs/adr/0023-users-and-ownership.md)

Approach:

- [8. Memory is a shared service](docs/adr/0008-shared-memory-service.md)
- [9. Agents are integrated through ACP](docs/adr/0009-acp.md)
- [10. Sessions always run in sandbox pods](docs/adr/0010-sessions-run-in-pods.md)
- [11. Enforcement happens at the pod boundary](docs/adr/0011-enforcement-at-the-pod-boundary.md)
- [12. Tools come from mise via a trusted installer](docs/adr/0012-tooling-via-mise.md)
- [13. Container workloads run inside or beside the session pod](docs/adr/0013-container-workloads.md)
- [19. Repositories come from runtime-managed mirrors](docs/adr/0019-repositories-from-runtime-mirrors.md)
- [20. Inference accounts can be switched under a running session](docs/adr/0020-switchable-inference-accounts.md)
- [21. Credentials live in the runtime](docs/adr/0021-credentials-live-in-the-runtime.md)
- [22. memini keys per session](docs/adr/0022-memini-keys-per-session.md)
- [24. Proposals are reviewed by another model before a human sees them](docs/adr/0024-automated-review-before-human-review.md)
- [26. The runtime makes small model calls of its own](docs/adr/0026-runtime-inference.md)

Implementation:

- [14. The runtime is written in Rust](docs/adr/0014-rust.md)
- [15. Postgres is the source of truth](docs/adr/0015-postgres.md)
- [16. Session pods are agent-sandbox Sandboxes](docs/adr/0016-agent-sandbox.md)
- [17. An in-pod supervisor bridges the agent to the runtime](docs/adr/0017-in-pod-supervisor.md)
- [18. The UI is Svelte with @immich/ui](docs/adr/0018-ui.md)

## License

[AGPL-3.0](LICENSE)
