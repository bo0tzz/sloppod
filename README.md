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
