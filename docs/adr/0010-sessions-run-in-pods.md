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
- **Resources** are requestable ([ADR 2](0002-capabilities.md)) and resized in place where possible. If a node can't fit the new size, the session is moved to another node: the transcript and session branch make it portable.

### Consequences

- Good: one execution model, and every session gets isolation and limits.
- Good: sessions no longer depend on the workstation being up.
- Bad: interactive host work goes over SSH, and anything needing a GUI on the host needs its own gated capability.
- Bad: cluster capacity becomes the limit on how many sessions can run at once.

## More Information

Spike, 2026-10-26, Kubernetes v1.37: an in-place resize (CPU and memory, `restartPolicy: NotRequired`) applied without a restart, and the container's cgroup limit followed. An impossible size was rejected at admission ("node didn't have enough allocatable resources"), a clear signal to move the session instead. In-place pod resize is GA since Kubernetes 1.35.
