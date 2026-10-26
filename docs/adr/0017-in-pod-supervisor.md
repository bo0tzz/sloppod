# 17. An in-pod supervisor bridges the agent to the runtime

- Status: Proposed
- Date: 2026-10-26

## Context and Problem Statement

The agent adapter speaks ACP over stdio inside the session pod. The runtime needs to reach it. Something also has to restore and sync the agent's transcript blob ([ADR 1](0001-sessions-decoupled-from-directories.md)), apply per-session config after start ([ADR 16](0016-agent-sandbox.md)), and set up the checkout and tools.

## Considered Options

- The runtime runs the adapter through `kubectl exec`-style stdio streams
- A small supervisor in the pod that connects outward to the runtime

## Decision Outcome

Proposed option: **an in-pod supervisor**. It is part of the runtime codebase and runs as the pod's entrypoint. It:

1. Connects outward to the runtime over WebSocket, authenticated with a session-scoped identity, and reconnects with backoff.
2. Fetches the session config: instructions, settings, the transcript blob, the checkout spec and the tool set.
3. Prepares the pod: restores the transcript, checks out the session branch, links the read-only tool store.
4. Starts the agent adapter, bridges its stdio ACP to the runtime, and passes steering through.
5. Syncs the transcript blob back at the end of each turn and before parking, and pushes work in progress to the session branch when parking.

Being outbound-only, the supervisor needs no ingress to session pods. That fits default-deny policies and doesn't need the agent-sandbox router.

### Consequences

- Good: the runtime never needs exec access into pods, and the pod lifecycle is owned by one component.
- Bad: another binary to ship in the session image.
