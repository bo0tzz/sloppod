# Deploying sloppod

sloppod ships as an OCI Helm chart, `oci://ghcr.io/bo0tzz/charts/sloppod`, published from `main` together with two images:

- `ghcr.io/bo0tzz/sloppod`: the runtime and UI
- `ghcr.io/bo0tzz/sloppod-session`: the session image with the supervisor, the Claude Code adapter, mise and the memini plugin

Published chart versions are `0.1.0-main.<run>`, with `appVersion` set to the commit; the images are tagged with the commit and `latest`. Installing the chart from a checkout instead needs `image.tag` and `session.image.tag` set, because the in-repo `appVersion` (`0.1.0`) isn't an image tag.

## Cluster prerequisites

- **Cilium with CiliumEnvoyConfig support:** Cilium's own Helm value `envoyConfig.enabled=true` (not a sloppod value). `dev/cilium-values.yaml` is a known-good Cilium configuration. Nodes need `net.ipv4.conf.all.src_valid_mark=0`, or every L7 redirect is dropped ([ADR 11](adr/0011-enforcement-at-the-pod-boundary.md)).
- **agent-sandbox v1.0.8:** `https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v1.0.8/sandbox.yaml`. Only the core `Sandbox` CRD and controller are needed.
- **Postgres**, e.g. a CloudNativePG `Cluster`. The chart reads a connection URI from a Secret, such as CNPG's `<cluster>-app` Secret, key `uri`.
- **A default StorageClass** with `ReadWriteOnce` volumes, or set `session.storage.storageClass`. Each session gets its own volume.
- **Network policies:** if the namespace has default-deny policies, allow:
  - session pods → the runtime on port 8080
  - Cilium's Envoy → the runtime on port 9001 (`ext_authz`; Envoy runs on the host network, so it shows up as `host`/`remote-node`), and the kubelet → port 8080 for probes
  - the runtime → the Kubernetes API, GitHub and Postgres

  sloppod writes its own CiliumNetworkPolicy per session pod.

## Secrets

The chart needs two existing Secrets; it creates none.

| Value | Key | Contents |
|---|---|---|
| `database.existingSecret` | `uri` | Postgres URI. |
| `masterKey.existingSecret` | `master-key` | 32 random bytes, base64 (`openssl rand -base64 32`). It encrypts the credentials sloppod stores ([ADR 21](adr/0021-credentials-live-in-the-runtime.md)); losing it makes them unreadable. |

Everything else is added in the UI, under Settings, after the first login:

- **A Claude account:** a long-lived token from `claude setup-token`. It stays in the runtime; session pods only see a placeholder.
- **A GitHub App:** its app ID and private key (PEM). Sessions act through it and it opens approved pull requests, so it needs **Contents: read & write**, **Pull requests: read & write** and **Metadata: read**, installed on every repository sessions may work on. The repositories a first message can pick from are its installations. (`sl0pz`, the app sloppod itself is developed with, has these on `bo0tzz/sloppod`.)
- **memini, optionally:** its URL, an admin key, and your personal namespace (e.g. `personal/bo0tzz`). The runtime uses the admin key only to mint one key per session ([ADR 22](adr/0022-memini-keys-per-session.md)).

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
    masterKey:
      existingSecret: sloppod-secret
    session:
      model: claude-opus-5-5
      memory: 8Gi
    # Optional: your user-level CLAUDE.md, rendered into every session.
    # instructions: |
    #   ...
```

The release name decides resource names. With release `sloppod`, the runtime Service is `sloppod`, port `http` (8080).

## Exposing the UI

The runtime has **no authentication of its own**. Every request acts as the user named in the `X-Forwarded-User` header (`auth.userHeader`), so put sloppod behind an authenticating proxy that sets it and strips it from client requests; with Envoy Gateway, an OIDC `SecurityPolicy` whose `jwt.claimToHeaders` maps `preferred_username` to that header. Users only see their own sessions and credentials ([ADR 23](adr/0023-users-and-ownership.md)). The UI needs WebSockets (`/acp`).

Route only port 8080 (`http`). Port 8081 (`sessions`) serves the supervisor link and the MCP endpoint for session pods, which can reach nothing else on the runtime; port 9001 is Envoy's authorizer. The chart's network policy enforces this inside the cluster.

## Checking it works

1. The runtime logs `sloppod runtime up`. It creates the `<release>-egress-ca` Secret and the `sloppod-egress` CiliumEnvoyConfig on first start.
2. In the UI, add a Claude account and a GitHub App under Settings, then start a session with a small first message that names a repository. Within a minute or two the session goes `starting` → `idle`/`working`. The first start of a repository with a `mise.toml` takes longer, while tools install into the session's volume.
3. If it stays `starting`, the session pod's log (the `session` container) shows the supervisor: link errors, clone or mise failures, and the agent's stderr.
4. Denied egress shows up in the runtime log as `deny` lines with the session, host and path.
