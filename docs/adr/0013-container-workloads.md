# 13. Container workloads run inside or beside the session pod

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

About 2k container commands in past sessions break down as:

- throwaway Postgres instances
- compose dev stacks that bind-mount the source tree being edited
- one-off `docker run`s
- around 100 image builds
- a few kind clusters for developing operators

Very little of this needs a Docker daemon as such. Session pods are unprivileged.

## Decision Outcome

| Use | Approach |
|---|---|
| Throwaway Postgres, one-off images | Plain pods in a per-session namespace (the `namespace` capability), or `podman run` inside the session pod |
| Compose dev stacks | Rootless podman and `podman compose` inside the session pod, so bind-mounts of the working tree keep working |
| Image builds | `podman build`/buildah inside the pod, or a shared rootless buildkitd that sessions reach over mTLS |
| kind | envtest for integration tests; vCluster OSS (namespace-scoped) for e2e |
| Untrusted images | gVisor RuntimeClass |
| Full dockerd or kind as a last resort | Kata (privileged only inside the VM) |

The per-session namespace is created only when a session needs extra workloads. It gets:

- a ResourceQuota and LimitRange
- PodSecurity `baseline`
- a Role with no write access to NetworkPolicy or quota objects
- egress limited to the session

### Consequences

- Good: no privileged pods; the same isolation model applies to everything.
- Bad: depends on Kubernetes user namespaces (`hostUsers: false`). On Talos this needs `user.max_user_namespaces` raised from its hardened default of 0. Rootless podman also needs a Localhost seccomp profile that allows unshare/mount, and `/dev/net/tun` (via a device plugin) for bridge networking.
- Bad: kaniko is archived and sysbox has no Talos extension, so neither is an option.

## More Information

On Talos v1.14 a `hostUsers: false` pod failed with "no space left on device" from unshare, because `user.max_user_namespaces=0`. Raising it is an accepted dependency, not yet applied.

Research: Kubernetes user namespaces are GA since 1.36. Talos ships kernel 6.18, containerd 2.3 and runc 1.5. vCluster OSS v0.37 is Apache-2.0. gVisor and Kata are official Talos extensions.
