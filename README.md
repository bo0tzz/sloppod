# sloppod

A runtime for coding-agent sessions. Sessions run in sandbox pods on Kubernetes, are not tied to any directory, and get access to things through composable, requestable capabilities. Outward-facing actions go through human review unless a standing approval covers them.

## Docs

### Architecture decisions

Concepts and approach:

1. [Sessions are decoupled from directories and owned by the runtime](docs/adr/0001-sessions-decoupled-from-directories.md)
2. [Capabilities are composable, requestable grants](docs/adr/0002-capabilities.md)
3. [Outward actions are proposals executed by the runtime](docs/adr/0003-proposals-and-trusted-executor.md)
4. [Interaction is asynchronous](docs/adr/0004-asynchronous-interaction.md)
5. [Sessions can be parked and woken](docs/adr/0005-park-and-wake.md)
6. [The runtime is self-referential](docs/adr/0006-self-reference.md)
7. [Extension points have plugin-shaped boundaries](docs/adr/0007-extension-points.md)
8. [Memory is a shared service](docs/adr/0008-shared-memory-service.md)
9. [Agents are integrated through ACP](docs/adr/0009-acp.md)
10. [Sessions always run in sandbox pods](docs/adr/0010-sessions-run-in-pods.md)
11. [Enforcement happens at the pod boundary](docs/adr/0011-enforcement-at-the-pod-boundary.md)
12. [Tools come from mise via a trusted installer](docs/adr/0012-tooling-via-mise.md)
13. [Container workloads run inside or beside the session pod](docs/adr/0013-container-workloads.md)
