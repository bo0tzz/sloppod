# Deploying sloppod

sloppod ships as an OCI Helm chart, `oci://ghcr.io/bo0tzz/charts/sloppod`, published from `main` together with two images:

- `ghcr.io/bo0tzz/sloppod`: the runtime and UI
- `ghcr.io/bo0tzz/sloppod-session`: the session image with the supervisor, the Claude Code adapter, mise and the memini plugin. A new session image reaches running sessions as they go quiet: a session idle for 15 minutes, with no turn, nothing queued, no open request and none of its agent's commands running, whose pod runs another image, gets a new pod, and its agent resumes the conversation. A session whose supervisor predates reporting its commands waits for 2 hours of quiet instead

Releases are `v<version>` tags: chart `<version>` on images tagged `v<version>` (and `latest`). Every push to `main` also publishes chart `<last release, next patch>-main.<run>` (e.g. `0.1.1-main.12` after `v0.1.0`) on images tagged with the commit (and `main`); semver ranges without a prerelease part skip those. A release commit sets the version in `Chart.yaml` and `Cargo.toml`; the tag has to match both. Installing the chart from a checkout instead needs `image.tag` and `session.image.tag` set, because the in-repo `appVersion` isn't an image tag.

## Cluster prerequisites

- **Cilium with CiliumEnvoyConfig support:** Cilium's own Helm value `envoyConfig.enabled=true` (not a sloppod value). `dev/cilium-values.yaml` is a known-good Cilium configuration. Nodes need `net.ipv4.conf.all.src_valid_mark=0`, or every L7 redirect is dropped ([ADR 11](adr/0011-enforcement-at-the-pod-boundary.md)).
- **Session DNS:** sessions may look up only the hosts they may reach and names under the cluster domain (`clusterDomain`, default `cluster.local`); Cilium's DNS proxy refuses the rest. Resolvers stop at a refused search-list expansion, so if nodes add search domains of their own to pods' `resolv.conf`, set Cilium's `dnsProxy.dnsRejectResponseCode=nameError`, or sessions can't resolve anything outside the cluster.
- **Fast failure for refused connections (recommended):** Cilium's `policyDenyResponse=icmp` (experimental in 1.20) answers egress that policy refuses with an ICMP Destination Unreachable instead of dropping it. Session policies let that ICMP in. Without it, a connection to a host the session can resolve but not reach on that port, or to a bare IP, hangs until the client times out. Blocked names already fail fast, because they don't resolve.
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
- **A GitHub App:** its app ID and private key (PEM). Sessions act through it, commit as its bot user (`<slug>[bot]`), and it opens approved pull requests and files approved issues, so it needs **Contents: read & write**, **Pull requests: read & write**, **Issues: read & write** and **Metadata: read**, installed on every repository sessions may work on. The repositories a first message can pick from are its installations. (`sl0pz`, the app sloppod itself is developed with, has these on `bo0tzz/sloppod`.)
- **A ChatGPT login, optionally:** for Codex reviews of proposals ([ADR 24](adr/0024-automated-review-before-human-review.md)). Log in with a fresh Codex home (`CODEX_HOME=$(mktemp -d) codex login`) and paste that directory's `auth.json`. The runtime keeps the tokens and refreshes them; refresh tokens are single-use, so don't use that login anywhere else. Sessions get a `codex` grant, and the authorizer attaches the token to Codex's API on `chatgpt.com`; session pods only see a placeholder.
- **memini, optionally:** its URL, an admin key, and your personal namespace (e.g. `personal/bo0tzz`; sessions read it, and those without a repository write under it). Only the runtime talks to memini, sessions go through it, so prefer the in-cluster address (e.g. `http://memini.slop.svc.cluster.local:8080`). The runtime uses the admin key only to mint one key per session ([ADR 22](adr/0022-memini-keys-per-session.md)).

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
    tag: "0.1.0"
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

The runtime has **no authentication of its own**. Every request acts as the user named in the `X-Forwarded-User` header (`auth.userHeader`), so put sloppod behind an authenticating proxy that sets it and strips it from client requests; with Envoy Gateway, an OIDC `SecurityPolicy` whose `jwt.claimToHeaders` maps `preferred_username` to that header. Users only see their own sessions and credentials ([ADR 23](adr/0023-users-and-ownership.md)). The UI needs WebSockets (`/acp`). From a browser, only sloppod's own pages can write through the API or open `/acp`: a request's `Origin` must be the scheme and host it was made to, as the proxy passes them on (`X-Forwarded-Proto` and `X-Forwarded-Host`, or `http` and `Host` without a proxy), so another site's page can't use the proxy's cookie to act as the user. Have the proxy set `X-Forwarded-Proto`, or HTTPS pages are refused. Serve the UI from another origin only by adding it to `auth.allowedOrigins`.

Route only port 8080 (`http`). Port 8081 (`sessions`) serves the supervisor link and the MCP endpoint for session pods, which can reach nothing else on the runtime; port 9001 is Envoy's authorizer. The chart's network policy enforces this inside the cluster.

## Checking it works

1. The runtime logs `sloppod runtime up`. It creates the `<release>-egress-ca` Secret and the `sloppod-egress` CiliumEnvoyConfig on first start.
2. In the UI, add a Claude account and a GitHub App under Settings, then start a session with a small first message that names a repository. Within a minute or two the session goes `starting` → `idle`/`working`. The first start of a repository with a `mise.toml` takes longer, while tools install into the session's volume.
3. If it stays `starting`, the session pod's log (the `session` container) shows the supervisor: link errors, clone or mise failures, and the agent's stderr.
4. Denied egress shows up in the runtime log as `deny` lines with the session, host and path.
