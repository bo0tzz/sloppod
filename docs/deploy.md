# Deploying sloppod

sloppod ships as an OCI Helm chart, `oci://ghcr.io/bo0tzz/charts/sloppod`, published from `main` together with two images:

- `ghcr.io/bo0tzz/sloppod`: the runtime and UI
- `ghcr.io/bo0tzz/sloppod-session`: the session image with the supervisor, the Claude Code adapter, mise and the memini plugin

Chart versions are `0.1.0-main.<run>`. `appVersion` is the commit, and the images are tagged with it.

## Cluster prerequisites

- **Cilium with CiliumEnvoyConfig support:** Helm value `envoyConfig.enabled=true`. Nodes need `net.ipv4.conf.all.src_valid_mark=0`, or every L7 redirect is dropped ([ADR 11](adr/0011-enforcement-at-the-pod-boundary.md)).
- **agent-sandbox v1.0.8:** `https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v1.0.8/sandbox.yaml`. Only the core `Sandbox` CRD and controller are needed.
- **Postgres**, e.g. a CloudNativePG `Cluster`. The chart reads a connection URI from a Secret, such as CNPG's `<cluster>-app` Secret, key `uri`.
- **A default StorageClass** with `ReadWriteOnce` volumes, or set `session.storage.storageClass`. Each session gets its own volume.
- **Network policies:** if the namespace has default-deny policies, allow:
  - session pods → the runtime on port 8080
  - Cilium's Envoy → the runtime on port 9001 (`ext_authz`; Envoy runs on the host network, so it shows up as `host`/`remote-node`)
  - the runtime → the Kubernetes API, GitHub and Postgres

  sloppod writes its own CiliumNetworkPolicy per session pod.

## Secrets

All are referenced by name from values; the chart creates none.

| Value | Key | Contents |
|---|---|---|
| `claude.existingSecret` | `oauth-token` | A long-lived token from `claude setup-token`. It stays in the runtime; session pods only see a placeholder. |
| `github.existingSecret` | `private-key` | The GitHub App's private key (PEM). Set `github.appId` too. |
| `database.existingSecret` | `uri` | Postgres URI. |
| `memini.existingSecret` | `api-key` | Optional. A memini key for the runtime, e.g. `memini key add sloppod --home personal/bo0tzz`, so sessions read your personal memory. Set `memini.url` too. |

The GitHub App is what sessions act through, and what opens approved pull requests. It needs **Contents: read & write**, **Pull requests: read & write** and **Metadata: read**, and must be installed on every repository sessions may work on. The repositories offered in the UI are the app's installations. `sl0pz` works for `bo0tzz/sloppod` as is.

## HelmRelease (Flux)

```yaml
apiVersion: source.toolkit.fluxcd.io/v1
kind: OCIRepository
metadata:
  name: sloppod
spec:
  interval: 10m
  url: oci://ghcr.io/bo0tzz/charts/sloppod
  ref:
    semver: ">=0.1.0-0"
---
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: sloppod
spec:
  interval: 10m
  chartRef:
    kind: OCIRepository
    name: sloppod
  values:
    database:
      existingSecret: sloppod-db-app
    claude:
      existingSecret: sloppod-claude
    github:
      appId: "5484236"
      existingSecret: sloppod-github
    memini:
      url: https://memini.bo0tzz.me
      existingSecret: sloppod-memini
    session:
      model: claude-opus-5-5
      memory: 8Gi
      # `auto` lets Claude Code's classifier handle permission prompts; the pod boundary is the hard limit.
      defaultMode: auto
    instructions: |
      # (your user-level CLAUDE.md, rendered into every session)
```

The release name decides resource names. With release `sloppod`, the runtime Service is `sloppod`, port `http` (8080).

## Exposing the UI

The runtime has **no authentication of its own**: anyone who can reach port 8080 can start sessions with your Claude subscription and your GitHub App. Expose it only behind an authenticating proxy, e.g. an HTTPRoute on the internal gateway with Authentik forward auth. The UI needs WebSockets (`/acp`).

Never expose `/link/v1`, `/mcp` or port 9001 publicly. They authenticate session pods by ServiceAccount token or pod IP, but they're internal endpoints.

## Checking it works

1. The runtime logs `sloppod runtime up`. It creates the `sloppod-ca` Secret and the `sloppod-egress` CiliumEnvoyConfig on first start.
2. In the UI, start a session on a repository with a small prompt. Within a minute or two the session goes `starting` → `idle`/`working`. The first start of a repository with a `mise.toml` takes longer, while tools install into the session's volume.
3. If it stays `starting`, the session pod's log (the `session` container) shows the supervisor: link errors, clone or mise failures, and the agent's stderr.
4. Denied egress shows up in the runtime log as `deny` lines with the session, host and path.
