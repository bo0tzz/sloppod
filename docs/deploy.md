# Deploying sloppod

sloppod ships as an OCI Helm chart, `oci://ghcr.io/bo0tzz/charts/sloppod`, published from `main` together with two images:

- `ghcr.io/bo0tzz/sloppod`: the runtime and UI
- `ghcr.io/bo0tzz/sloppod-session`: the session image with the supervisor, the Claude Code adapter, mise and the memini plugin. A new session image reaches running sessions as they go quiet: a session idle for 15 minutes, with no turn, nothing queued, no open request and none of its agent's commands running, whose pod runs another image, gets a new pod, and its agent resumes the conversation. While any session pod can't be placed for lack of resources, no pod is replaced (#454). A session whose agent was asked to make room for the image (its pod over a day old) gets one once it has been quiet for a minute, checked every minute. A session whose supervisor predates reporting its commands waits for 2 hours of quiet instead

Releases are `v<version>` tags: chart `<version>` on images tagged `v<version>` (and `latest`). Every push to `main` also publishes chart `<last release, next patch>-main.<run>` (e.g. `0.1.1-main.12` after `v0.1.0`) on images tagged with the commit (and `main`); semver ranges without a prerelease part skip those. A release commit sets the version in `Chart.yaml` and `Cargo.toml`; the tag has to match both. Installing the chart from a checkout instead needs `image.tag` and `session.image.tag` set, because the in-repo `appVersion` isn't an image tag. Each image records what it was built from, and the runtime reports it: a page's version shows a dot when the runtime serving it is a different build, so a tab left open across a deploy can be reloaded into the new bundle ([#448](https://github.com/bo0tzz/sloppod/issues/448)).

## Cluster prerequisites

- **Cilium with CiliumEnvoyConfig support:** Cilium's own Helm value `envoyConfig.enabled=true` (not a sloppod value). `dev/cilium-values.yaml` is a known-good Cilium configuration. Nodes need `net.ipv4.conf.all.src_valid_mark=0`, or every L7 redirect is dropped ([ADR 11](adr/0011-enforcement-at-the-pod-boundary.md)).
- **Session DNS:** sessions may look up only the hosts they may reach and names under the cluster domain (`clusterDomain`, default `cluster.local`); Cilium's DNS proxy refuses the rest. Resolvers stop at a refused search-list expansion, so if nodes add search domains of their own to pods' `resolv.conf`, set Cilium's `dnsProxy.dnsRejectResponseCode=nameError`, or sessions can't resolve anything outside the cluster.
- **Fast failure for refused connections (recommended):** Cilium's `policyDenyResponse=icmp` (experimental in 1.20) answers egress that policy refuses with an ICMP Destination Unreachable instead of dropping it. Session policies let that ICMP in. Without it, a connection to a host the session can resolve but not reach on that port, or to a bare IP, hangs until the client times out. Blocked names already fail fast, because they don't resolve.
- **agent-sandbox v1.0.8:** `https://github.com/kubernetes-sigs/agent-sandbox/releases/download/v1.0.8/sandbox.yaml`. Only the core `Sandbox` CRD and controller are needed.
- **Postgres**, e.g. a CloudNativePG `Cluster`. The chart reads a connection URI from a Secret, such as CNPG's `<cluster>-app` Secret, key `uri`.
- **A default StorageClass** with `ReadWriteOnce` volumes, or set `session.storage.storageClass`. Each session gets its own volume. Set `allowVolumeExpansion: true` on it so a session's volume can grow before it fills up (#479, below); without it, sessions that run out of room are told so and keep working until they're full. Have it support **cloning** (`PersistentVolumeClaim` volume data sources, `CloneVolume` on a CSI driver) so a sub-session can start from a copy of its parent's volume ([#478](https://github.com/bo0tzz/sloppod/issues/478), below); where it doesn't, those children are placed with a fresh volume — which the parent is told when it spawns them — and install their own tools and caches.
- **Network policies:** if the namespace has default-deny policies, allow:
  - session pods → the runtime on port 8080
  - Cilium's Envoy → the runtime on port 9001 (`ext_authz`; Envoy runs on the host network, so it shows up as `host`/`remote-node`), and the kubelet → port 8080 for probes
  - the runtime → the Kubernetes API, GitHub and Postgres

  sloppod writes its own CiliumNetworkPolicy per session pod.
- **Room for session pods:** each session pod requests its `session.memory` (and CPU) at the same priority as the others, so Kubernetes doesn't preempt one for another. When a session's pod can't be scheduled for lack of resources, the runtime parks quiet sessions to make room for it and holds back image rolls ([ADR 25](adr/0025-session-lifecycle.md)); the session shows why it waits. Size the nodes for the sessions you expect to have working at once; parked ones take no room. The runtime reads pods' `PodScheduled` condition, through its watch on session pods.

## Secrets

The chart needs two existing Secrets; it creates none.

| Value | Key | Contents |
|---|---|---|
| `database.existingSecret` | `uri` | Postgres URI. |
| `masterKey.existingSecret` | `master-key` | 32 random bytes, base64 (`openssl rand -base64 32`). It encrypts the credentials sloppod stores ([ADR 21](adr/0021-credentials-live-in-the-runtime.md)); losing it makes them unreadable. |

Everything else is added in the UI, under Settings, after the first login:

- **A Claude account:** a long-lived token from `claude setup-token`. It stays in the runtime; session pods only see a placeholder.
- **A GitHub App:** its app ID and private key (PEM). Sessions act through it, commit as its bot user (`<slug>[bot]`), and it opens approved pull requests and files approved issues, so it needs **Contents: read & write**, **Pull requests: read & write**, **Issues: read & write** and **Metadata: read**, installed on every repository sessions may work on. With **Workflows: read & write** as well, sessions can also push changes to `.github/workflows`; without it, GitHub refuses those pushes. The repositories a first message can pick from are its installations. (`sl0pz`, the app sloppod itself is developed with, has these on `bo0tzz/sloppod`.)
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
    # Optional: instructions for every user's sessions, rendered ahead of each user's own (Settings).
    # For one user, Settings → Your instructions is the place.
    # instructions: |
    #   ...
```

The release name decides resource names. With release `sloppod`, the runtime Service is `sloppod`, port `http` (8080).

## Model endpoints

Sessions can run Claude Code on models of any Anthropic-compatible endpoint ([#461](https://github.com/bo0tzz/sloppod/issues/461)), such as a [LiteLLM](https://github.com/BerriAI/litellm) proxy in front of other providers or a model served in the cluster. Add one in Settings → Model endpoints: its base URL (e.g. `http://litellm.ai.svc.cluster.local:4000`), the model names it serves, and its key. Its models are then offered wherever a model is chosen. Sessions reach it through the runtime's sessions port, which adds the key, so the endpoint only has to be reachable from the runtime pod, by a cluster Service name or an internet host, over plain HTTP or HTTPS; nothing in the session's egress policy changes. The session image has to be one from this release or later: its supervisor serves the `/inference` route the agent is pointed at, and older pods are moved onto the new image as they go quiet (above).

**A runtime restart during a turn on an endpoint model.** Model answers stream through the runtime, so stopping it would cut them. On `SIGTERM` the runtime stops taking new model requests (they get `503` with `retry-after`) and waits up to 60 seconds for answers in flight to finish before exiting; the chart sets `terminationGracePeriodSeconds: 75` to leave room for it. An answer still streaming after that is cut. Claude Code tries a cut answer again at once, and a refused or unreachable request again with backoff for about three minutes (checked against Claude Code 2.1.310), so a turn carries on once the next runtime is up: what it loses is the tokens spent on the cut answer, which the endpoint bills again. With the chart's `Recreate` strategy the next runtime starts only after this one exits, so a deploy takes up to a minute longer while answers finish. Sessions on Claude or Codex aren't affected: they reach their providers through the egress listener, not the runtime.

**Context windows.** Claude Code doesn't know an endpoint's models, so without a window it assumes its default and compacts against that, often far too early. Give each model's window with the endpoint (Context windows, e.g. `deepseek-v4=1000000`); a LiteLLM operator model's comes from its `spec.info.maxInputTokens` (below). Sessions get it as `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, and Claude Code's auto-compaction follows it. A model without one keeps Claude Code's default and its warning about it. A manual endpoint that is a LiteLLM proxy isn't asked for its `/model/info`; give the windows by hand.

**Auto mode on an endpoint model.** In auto mode Claude Code asks Anthropic's API to classify tool calls on the server, with the `auto-mode-classifier` beta (the change behind [auto-mode classifier billing](https://code.claude.com/docs/en/auto-mode-classifier-billing)). An endpoint that isn't Anthropic's answers without a verdict, so Claude Code would fall back to classifying with its own requests and warn that the session "isn't eligible because your requests go through 127.0.0.1:7811". There's nothing for the runtime's `/inference` route to implement there: the verdict comes from Anthropic's model, and the endpoint's model classifies anyway. So sessions on an endpoint model run with `CLAUDE_CODE_AUTO_MODE_SERVER=0`, which has Claude Code classify locally, on the endpoint's model, from the start, without the warning. Sessions on a Claude account reach Anthropic's API directly (through the egress proxy, not `/inference`), so the server classifier works for them and they don't see it.

### With the LiteLLM operator

With the [LiteLLM operator](https://github.com/home-operations/litellm-operator) installed, its proxies and models are found by themselves: by default in sloppod's own namespace, or in the namespaces listed under `litellm.namespaces` (`[]` turns it off), and every user is offered each proxy's models. Where the operator isn't installed, the runtime logs once, at info, that it found no LiteLLM operator CRDs, and looks again every 10 minutes, so installing the operator later needs no restart. A session on one gets a `LiteLLMVirtualKey` of its own in sloppod's namespace, limited to its model, and the key goes when the session stops or is purged. The chart then grants the runtime `get`, `list` and `watch` on `litellmproxies` and `litellmmodels` in those namespaces, and in its own namespace `get`, `list`, `create`, `patch` and `delete` on `litellmvirtualkeys` (an hourly sweep lists them to delete any no session holds) and `get` on Secrets, where the operator writes the keys it mints.

```yaml
# The HelmRelease's values, when the proxies aren't in sloppod's own namespace (the default):
litellm:
  namespaces: [ai]
---
apiVersion: litellm.home-operations.com/v1alpha1
kind: LiteLLMProxy
metadata:
  name: main
  namespace: ai
spec:
  # Virtual keys are minted through the proxy's admin API, with its database.
  apiAccess:
    masterKeyRef:
      name: litellm-secrets
      key: LITELLM_MASTER_KEY
  generalSettings:
    database_url: os.environ/DATABASE_URL
  envFrom:
    - secretRef:
        name: litellm-secrets
---
# A hosted model, through its provider's API.
apiVersion: litellm.home-operations.com/v1alpha1
kind: LiteLLMModel
metadata:
  name: glm-5-2
  namespace: ai
spec:
  modelName: glm-5.2
  params:
    model: openai/glm-5.2
    apiBase: https://api.z.ai/api/coding/paas/v4
    apiKeyRef:
      name: litellm-secrets
      key: ZAI_API_KEY
  info:
    maxInputTokens: 200000
    supportsFunctionCalling: true
---
# A model served in the cluster, e.g. by vLLM.
apiVersion: litellm.home-operations.com/v1alpha1
kind: LiteLLMModel
metadata:
  name: qwen3-coder
  namespace: ai
spec:
  modelName: qwen3-coder
  params:
    model: hosted_vllm/Qwen/Qwen3-Coder-30B-A3B-Instruct
    apiBase: http://vllm.ai.svc.cluster.local:8000/v1
  info:
    maxInputTokens: 131072
    supportsFunctionCalling: true
```

Sessions reach the proxy's Service (`main.ai.svc.cluster.local:4000` here) through the runtime, so the runtime pod needs to reach it; a session's own egress doesn't change. A model is offered as soon as the operator would bind it to a proxy, and isn't any more once it wouldn't. Each session's spend, from its key's `/key/info`, shows next to the usage meters with the user's total, and a budget per session set in Settings (Defaults for new sessions) is set as each new key's `maxBudget`, which LiteLLM enforces.

## What a session keeps

What each session holds in the cluster, by its phase ([ADR 25](adr/0025-session-lifecycle.md), #455):

- **Running** (starting, idle, working, waiting): its Sandbox and pod, its workspace volume, its CiliumNetworkPolicy and CiliumEnvoyConfig listener, and its keys.
- **Parked:** its Sandbox (suspended) and volume. The policy fences the pod in while it stops and is removed once the pod is gone; waking applies it again, and mints its keys if they're gone.
- **Stopped** (it can't start again): its pod is ended, fenced in while it goes, and then its policy, listener and keys go. Its volume, with the suspended Sandbox that owns it, goes a week after it stopped, unless the user keeps it (Settings → Storage).
- **Deleted:** everything stays, parked, for the 7 days it can be restored, then goes. A review session that's done goes at once.

In every phase the transcript, proposals and branch stay: they live in the database and on GitHub. An hourly sweep removes Sandboxes, volume claims, policies and listeners whose session the database knows and has purged, such as ones whose removal failed, and logs each (`removed the objects of a purged session`). Objects of sessions the database doesn't know at all are left alone: started against an empty or restored database, a wrong `sloppod-db` Secret, or one shared with another install in the namespace, every session would look unknown, and removing them would lose their work. The sweep logs them instead, once an hour with a count and the ids (`objects of sessions the database doesn't know; left alone`). If they really are leftovers, remove them by hand: `kubectl delete sandbox sloppod-<id>`, `kubectl delete pvc workdir-sloppod-<id>`, `kubectl delete ciliumnetworkpolicy sloppod-session-<id>` and `kubectl delete ciliumenvoyconfig sloppod-egress-<id>`. Each session's supervisor reports its volume's use every 5 minutes and once more as its pod stops, so a parked session shows what it keeps; the session list, a session's header and Settings → Storage show it. Settings → Storage lists every volume the cluster holds for the user's sessions (`GET /storage`, from the placement backend's claims): reviewers' and deleted ones' too, one never measured (parked before sessions reported) as "not measured", and, in a group of their own, those of sessions the database doesn't know. Its count of volumes is that of `kubectl get pvc` for `workdir-sloppod-*` claims, for one user.

A session's volume also grows before it fills up (#479). When a report says `session.storage.growAt` (default `0.85`, so 85%) of it is used, the runtime patches the claim to `growBy` (default `0.5`, so half again) more, at least 5Gi, up to `session.storage.maxSize` (default `100Gi`). More room is only asked for once the cluster has given what it last asked for: while the claim asks for more than it has, or a resize condition stands (`Resizing`, `FileSystemResizePending`), the report is left for the next one. The session is told when its volume grew. Where it can't grow any more — the storage class doesn't allow expansion, so the cluster refuses the patch, or the volume is at `maxSize` — the session is told once that its workspace is full and to clean build output or caches, not at every report; a workspace that's cleaned up below the threshold and fills up again is told again. While the cluster still owes the room it was asked for, the size the claim asks for isn't room the session can use yet, so what the pod's own filesystem reports is measured instead: one that's all but full is told once to clean up rather than left waiting for room that may need a new pod. A session that isn't acting — parked, stopped or deleted — is told in its transcript rather than through its agent, since a notice kept for an absent agent wakes the session when the queues are restored; one still full when it runs again is told then. The runtime needs `get`, `list`, `delete` and `patch` on `persistentvolumeclaims`, which the chart grants.

A sub-session's workspace can start as a **copy** of its parent's volume ([#478](https://github.com/bo0tzz/sloppod/issues/478)). `spawn_session`'s `workspace: "copy"` has the child's claim cloned from the parent's (`dataSource: {kind: PersistentVolumeClaim, name: workdir-sloppod-<parent>}`, which the storage class has to support: `CloneVolume` on a CSI driver), at least as big as the one it copies; `"fresh"` gives it a volume of its own. It's the default for a child working on the parent's repository and fresh for one working elsewhere, and the parent's answer says which the child got — including when a copy it asked for couldn't be made, which is where a storage class with no clone support shows. The child gets the parent's checkout, tools, dependencies and caches, and nothing of the parent's identity or work: before its agent starts, its supervisor puts the checkout on the child's own branch from the parent's pushed head, without the parent's uncommitted, untracked or stashed changes, and clears what the parent left of its agent state, transcripts and keys. A clone that never provisions — the storage class refuses it, or it keeps failing — is replaced by a fresh volume a couple of minutes in, and the child is told; Settings → Storage shows the claim either way.

## Exposing the UI

The runtime has **no authentication of its own**. Every request acts as the user named in the `X-Forwarded-User` header (`auth.userHeader`), so put sloppod behind an authenticating proxy that sets it and strips it from client requests; with Envoy Gateway, an OIDC `SecurityPolicy` whose `jwt.claimToHeaders` maps `preferred_username` to that header. Users only see their own sessions and credentials ([ADR 23](adr/0023-users-and-ownership.md)). The UI needs WebSockets (`/acp`). From a browser, only sloppod's own pages can write through the API or open `/acp`: a request's `Origin` must be the scheme and host it was made to, as the proxy passes them on (`X-Forwarded-Proto` and `X-Forwarded-Host`, or `http` and `Host` without a proxy), so another site's page can't use the proxy's cookie to act as the user. Have the proxy set `X-Forwarded-Proto`, or HTTPS pages are refused. Serve the UI from another origin only by adding it to `auth.allowedOrigins`.

Route only port 8080 (`http`). Port 8081 (`sessions`) serves the supervisor link and the MCP endpoint for session pods, which can reach nothing else on the runtime; port 9001 is Envoy's authorizer. The chart's network policy enforces this inside the cluster.

## Checking it works

1. The runtime logs `sloppod runtime up`. It creates the `<release>-egress-ca` Secret and the `sloppod-egress` CiliumEnvoyConfig on first start.
2. In the UI, add a Claude account and a GitHub App under Settings, then start a session with a small first message that names a repository. Within a minute or two the session goes `starting` → `idle`/`working`. The first start of a repository with a `mise.toml` takes longer, while tools install into the session's volume.
3. If it stays `starting`, the session pod's log (the `session` container) shows the supervisor: link errors, clone or mise failures, and the agent's stderr.
4. Denied egress shows up in the runtime log as `deny` lines with the session, host and path.
