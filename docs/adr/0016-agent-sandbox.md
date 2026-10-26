# 16. Session pods are agent-sandbox Sandboxes

- Status: Proposed
- Date: 2026-10-26

## Context and Problem Statement

Each session needs a pod with stable identity and storage that survives parking. Fast starts would help, and the session's network policy must remain ours ([ADR 11](0011-enforcement-at-the-pod-boundary.md)).

## Considered Options

- Plain Pods and PVCs created by the runtime
- [kubernetes-sigs/agent-sandbox](https://github.com/kubernetes-sigs/agent-sandbox)

## Decision Outcome

Proposed option: **agent-sandbox**. It was at v1.0.8 on 2026-10-22, released weekly, and is a SIG Apps project.

How it maps to our needs:

- **`Sandbox`:** one stateful pod with a stable identity and `volumeClaimTemplates`.
- **Parking** ([ADR 5](0005-park-and-wake.md)) maps to `operatingMode: Suspended`, which deletes the pod and keeps the Sandbox and its PVCs. `shutdownTime` with `shutdownPolicy` handles expiry.
- **`SandboxTemplate`** holds the shared session pod spec. Set `networkPolicyManagement: Unmanaged` so it doesn't create its own NetworkPolicy; per-session Cilium policies stay ours.
- **`SandboxWarmPool` and `SandboxClaim`** give fast starts. Setting env vars or PVCs on a claim forces a cold start, so per-session config should be delivered after start by the in-pod supervisor ([ADR 17](0017-in-pod-supervisor.md)), not baked into the claim.
- **The optional sandbox router** isn't needed: the supervisor connects outward to the runtime.

### Consequences

- Good: suspend/resume, warm pools and expiry come for free, so the runtime doesn't have to manage pods itself.
- Bad: a young API that we depend on. Rust types are generated from its CRDs.

Before accepting, check against the current release:

- how claims and the warm pool behave with our flow of config pushed after start
- that `Unmanaged` network policy really leaves no NetworkPolicy behind
- how PVCs behave across suspend and resume on our storage
