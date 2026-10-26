# 10. Sessions always run in sandbox pods

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Agent sessions currently run directly on a workstation with the user's full privileges. That makes isolation, resource limits and moving sessions around hard, and ties sessions to the workstation being on.

## Considered Options

- Local mode plus pod mode, with the same image (e.g. rootless podman locally)
- Pods only, with the workstation reachable as a capability

## Decision Outcome

Chosen option: **pods only**. There is no local mode.

- **Placement:** every session runs in a sandbox pod on a Kubernetes cluster.
- **Host access:** work that needs a specific host (hardware, desktop, local devices) gets an `exec@<host>` capability, which issues a short-lived SSH certificate from the runtime's CA, limited to a principal. Placement is about cost and latency; what a session can reach is a separate question, answered by its capabilities.
- **Shutdown is mechanical.** Pods go away for many reasons: preemption, eviction, a move to another node, parking ([ADR 5](0005-park-and-wake.md)) or a restart ([ADR 6](0006-self-reference.md)). The agent can register **shutdown handlers** for background work it starts: a command, a timeout and a description. On termination the runtime:
  1. runs the handlers, without any model calls
  2. records their output, along with what was still running unregistered
  3. snapshots work in progress and syncs the transcript ([ADR 17](0017-in-pod-supervisor.md))

  The working tree itself lives on a per-session **workdir volume** that outlives the pod, so preemption or a move doesn't lose uncommitted work.

  When the session next runs, the agent finds a summary of all this in its inbox.
- **Resources** are requestable ([ADR 2](0002-capabilities.md)) and resized in place where possible. If a node can't fit the new size, the session is moved to another node: the transcript and session branch make it portable.

### Consequences

- Good: one execution model, and every session gets isolation and limits.
- Good: sessions no longer depend on the workstation being up.
- Bad: interactive host work goes over SSH, and anything needing a GUI on the host needs its own gated capability.
- Bad: cluster capacity becomes the limit on how many sessions can run at once.

## More Information

Spike, 2026-10-26, Kubernetes v1.37: an in-place resize (CPU and memory, `restartPolicy: NotRequired`) applied without a restart, and the container's cgroup limit followed. An impossible size was rejected at admission ("node didn't have enough allocatable resources"), a clear signal to move the session instead. In-place pod resize is GA since Kubernetes 1.35.
