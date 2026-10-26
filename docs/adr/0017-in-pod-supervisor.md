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
5. Syncs the transcript blob back at the end of each turn.
6. Keeps the session's shutdown handlers ([ADR 10](0010-sessions-run-in-pods.md)), registered by the agent through a runtime tool.

On termination (`SIGTERM`, whether from preemption, eviction, a move, parking or a restart), within the pod's grace period, the supervisor:

1. Interrupts the agent, so no new model calls are made.
2. Runs the shutdown handlers in parallel, each with its own timeout, within a fixed share of the grace period.
3. Records each handler's result, plus every still-running process it didn't know about (command line and tail of its output).
4. Commits and pushes work in progress to the session branch, and syncs the transcript blob.
5. Sends the runtime a shutdown report, which becomes an inbox item when the session next runs.

The grace period is sized for handlers plus sync; the default is 90 s. A pod killed without a signal skips all of this, so the runtime records a lost pod and the next run starts from the last sync.

Being outbound-only, the supervisor needs no ingress to session pods. That fits default-deny policies and doesn't need the agent-sandbox router.

### Consequences

- Good: the runtime never needs exec access into pods, and the pod lifecycle is owned by one component.
- Bad: another binary to ship in the session image.
