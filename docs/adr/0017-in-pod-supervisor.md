# 17. An in-pod supervisor bridges the agent to the runtime

- Status: Accepted
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
4. Snapshots work in progress to the runtime ([ADR 19](0019-repositories-from-runtime-mirrors.md)), and syncs the transcript blob.
5. Sends the runtime a shutdown report, which becomes an inbox item when the session next runs.

The grace period is sized for handlers plus sync; the default is 90 s. A pod killed without a signal skips all of this, so the runtime records a lost pod and the next run starts from the last sync.

Being outbound-only, the supervisor needs no ingress to session pods. That fits default-deny policies and doesn't need the agent-sandbox router.

### Consequences

- Good: the runtime never needs exec access into pods, and the pod lifecycle is owned by one component.
- Bad: another binary to ship in the session image.

## More Information

Milestone 1, 2026-10-26: the session-scoped identity is the pod's projected ServiceAccount token (audience `sloppod`, from a ServiceAccount with no permissions). The runtime checks it with TokenReview, whose `authentication.kubernetes.io/pod-name` extra names the pod, and the pod's label names the session. No per-session secret is delivered, and the same works for pods from a warm pool. On termination the supervisor stops the agent and syncs the transcript; shutdown handlers and the shutdown report are not built yet.

2026-10-27: the agent's lifetime is the pod's, not the link's. When the link drops (a runtime deploy, a network blip) the supervisor keeps the agent running and holds its output. It tracks the requests still open in each direction: the turn's `session/prompt`, permission requests and questions. On reconnect, its hello reports the running agent and those requests. The runtime then attaches instead of launching: it takes over the turn and shows the open questions again. An agent whose session wasn't up yet is launched afresh. The runtime restart notice goes only to agents that had to be launched again. Either end drops a link that has been silent for 60 s; both send pings every 20 s. The supervisor reconnects with backoff for as long as it runs. A liveness probe (`sloppod-supervisor --check`) fails once the runtime hasn't been heard from for 10 minutes, so a wedged supervisor or a link that stays down ends in a container restart.

2026-10-28: the supervisor fetches the transcript files it's missing from the runtime (`GET /link/v1/transcript/{name}`, with the pod's token), streamed, instead of getting every file inline in its launch message. Inline, a long session's launch was one message of tens of megabytes, built in the runtime's memory at each launch, even though the files are almost always still on the pod's volume ([#211](https://github.com/bo0tzz/sloppod/issues/211)). The launch names the files the runtime has; a supervisor that says it fetches them (`fetch_transcript` in its hello) gets the names only, and an older one still gets them inline.

2026-10-28: the first part of the shutdown report is built. On `SIGTERM`, before stopping the agent, the supervisor lists the agent's commands that are still running: each outermost process under the agent that started more than 10 s after it (the agent's own helpers, its CLI and MCP servers, start with it; commands only start in turns, and a command whose shell `exec`ed its program leaves no shell to recognize), with its command line and, when it writes to a file as background commands do, the last 20 lines it wrote. It sends them to the runtime (`ShutdownReport`), which queues a notice to the session: the agent hears what was cut off when it next runs, and can start it again. Shutdown handlers, and snapshotting work in progress, aren't built yet. A pod stopped while its link is down sends no report. The supervisor also reports how many such commands are running (`Commands`), whenever the number changes.
