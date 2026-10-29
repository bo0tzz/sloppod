# 11. Enforcement happens at the pod boundary

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Capabilities ([ADR 2](0002-capabilities.md)) need enforcement that works for any agent, and that a compromised or confused agent can't bypass. Agent-side hooks and permission rules are useful for judgment, but they are not a boundary. Credentials placed inside the sandbox can be copied out.

## Considered Options

- **Credentials in the pod**, scoped as tightly as possible.
- **Static injection at egress:** Cilium L7 rules with `headerMatches` + `secret` + `mismatch: REPLACE`. The pod holds a placeholder and Cilium overwrites it.
- **A per-request authorizer at egress:** Cilium redirects session traffic to an Envoy listener whose `ext_authz` filter asks sloppod about every request. The authorizer allows or denies it and attaches the credential.

## Decision Outcome

Chosen option: **a per-request authorizer at egress**.

- **Default-deny egress.** A session with no grants reaches nothing but its channel to the runtime ([ADR 2](0002-capabilities.md)). All allowed egress, including `web:read`, goes through the authorizer. For `web:read` the authorizer allows only `GET`/`HEAD`, attaches no credentials and, by default, strips any the agent supplies. DNS is allowlisted to match.
- **Credentials never enter the pod.** Session traffic to authenticated services is redirected (`toPorts[].listener` → a CiliumEnvoyConfig) to an Envoy listener that:
  1. terminates TLS with a certificate from the runtime's CA, which the pod trusts
  2. calls sloppod's **authorizer** through `ext_authz`
  3. re-originates TLS to the real upstream
- **The authorizer decides each request.** It sees the method, path, headers, and optionally the body. It allows or denies, and on allow attaches the credential, which can be minted per request or per short window. Its decisions come from the session's grants and the service's **request policy**, which is an extension point ([ADR 7](0007-extension-points.md)). Because it can see bodies, it can be content-aware where path rules can't:
  - GraphQL queries vs mutations, which share one endpoint
  - which refs a git push updates
  - rate limits, and an audit log of every authenticated call
- **Denials explain themselves.** The 403 body tells the agent what to do instead, e.g. use the propose tool ([ADR 3](0003-proposals-and-trusted-executor.md)).
- **Cilium's role** is the egress firewall (which hosts and ports a session may reach at all) and redirection to the listener. Cilium's own header injection is **not** used: with `REPLACE`, any request the rule allows gets the token, including an arbitrary `curl`, and keeping it alongside the authorizer would mean two ways to do the same thing.
- **Failure and cost:** the authorizer runs as part of the runtime. If the runtime is down, sessions can't do much anyway, so it gets no redundancy of its own. It fails closed (`failure_mode_allow: false`). Body inspection is opt-in per service: on for GitHub, off for the model API so large prompts aren't buffered.
- **Credentials the agent does legitimately hold:**
  - **Credentials for the session's own workloads,** such as a database or dev server in its pod, its namespace or its vCluster. That traffic never passes the authorizer.
  - **Short-lived, runtime-minted credentials for protocols the authorizer can't mediate,** such as SSH certificates. These are in the pod by design, tightly scoped and short-lived.
  - **Keys that are public by design,** such as search-only API keys or error-reporting DSNs. A request policy can allow specific credential headers or parameters to specific hosts.
  - **Secrets a human wants a session to use** are added to the runtime as a secret bound to a grant, not pasted into the chat (which would also send them to the model provider). The authorizer then attaches them like any other credential.
- **Model credential:** the agent's model credential goes through the same path, so the Claude OAuth token never enters the pod.
- **SSH:** Envoy has no SSH support, so it can gate connections but can't attach credentials. SSH starts with short-lived certificates from the runtime's CA (one principal, minutes of validity, `source-address`, optionally a forced command). An SSH-aware proxy (e.g. built on `russh`) is a later option, if command-level control is wanted.
- **Kubernetes:** API tokens come from TokenRequest bound to a grant object, so deleting the grant revokes them.
- **Enforcer extension point:** Cilium is the first enforcer. If a cluster can't redirect to a listener, the same Envoy and authorizer setup can run as a standalone egress proxy; only the data plane changes.

### Consequences

- Good: works the same for any agent, and decisions are made per request, with full context, by our code.
- Good: there are no credentials in the pod to exfiltrate, and revocation is immediate.
- Bad: every authenticated request costs one round trip to the authorizer, and the authorizer is critical: if it's down, no authenticated egress.
- Bad: we write raw Envoy configuration in CiliumEnvoyConfig, which Cilium barely validates. A listener redirect can't be combined with Cilium's own L7 rules on the same port.
- Bad: TLS interception requires pods to trust the runtime CA. Clients that pin certificates break.

### Limits

- **What a request does doesn't always match its method or path.** Kubernetes `exec`/`attach`/`port-forward` and service `proxy` are subresources, and `exec` over WebSocket starts as a `GET`. GitHub has POSTs that only read (`/markdown`, GraphQL queries), writes that bypass git (`PUT …/contents/{path}`), and `repository_dispatch`/`workflow_dispatch`. Request policies are therefore per-service allowlists written with that service's semantics, and the credential's own scope (RBAC, GitHub App permissions) is the outer bound. GraphQL needs real parsing.
- **Read-only web still leaks.** `web:read` prevents writes to the open web, but data can leave through query strings or DNS names. Prefer narrow variants (docs sites, package registries) over the open web, or reads through a runtime-side fetch tool.
- **Credentials found in content** (`.env` files, CI logs, git history) are stripped from requests to policed services. Elsewhere, default-deny egress is the only defense.
- **The agent's own tooling** can talk to its provider beyond inference (e.g. publishing Artifacts). The model policy allows only the inference endpoints unless a grant says otherwise.
- **SSH and other raw TCP** sit outside request-level control: `exec@<host>` is broad by nature, so it gets short TTLs, approval and forced commands where possible.

## More Information

Spike, 2026-10-26, kind with Cilium 1.20.4 (`envoyConfig.enabled=true`, needed for CiliumEnvoyConfig), with a stub authorizer in `ext_authz` HTTP mode:

| Request | Result |
|---|---|
| `GET /headers` with no auth header | allowed; `Bearer minted-by-authz-1` attached |
| `GET /headers` with the agent's own `Authorization` | allowed; the header was replaced by a fresh `…-2` |
| `POST /graphql` with a query in the body | allowed |
| `POST /graphql` with a mutation in the body | 403, with the authorizer's explanation passed through |
| A method or path with no rule | 403 |
| A host not in the egress policy | blocked |

Earlier spike, Cilium static injection: header replace/add, method/path allowlisting and TLS interception all worked. mTLS origination through `originatingTLS` does not work in SDS mode ([cilium#45632](https://github.com/cilium/cilium/issues/45632)). That doesn't affect this design, because our listener configures upstream TLS itself, and Envoy supports client certificates there. That's untested so far, but would cover mTLS services such as the Talos API, which is gRPC over mTLS: the authorizer could allow only read-only gRPC methods.

Gotcha: on hosts with `net.ipv4.conf.all.src_valid_mark=1` (common with Tailscale/WireGuard), every L7 redirect is dropped as `SKB_DROP_REASON_IP_LOCAL_SOURCE`. The kernel's source check uses the proxy's fwmark and hits Cilium's local routing table. Set it to 0 on nodes, including kind nodes.

Separately, a local reverse proxy that swapped a dummy `sk-ant-oat01-` token for the real one worked for Claude Code via `ANTHROPIC_BASE_URL`. Only `/v1/messages` went through it.

Spike, 2026-10-26: claude-agent-acp 0.97.0 (Claude Code 2.1.310, the SDK's bundled binary) behind a TLS-intercepting proxy, trusting its CA only through `NODE_EXTRA_CA_CERTS`, with a dummy `CLAUDE_CODE_OAUTH_TOKEN` swapped for the real one. Every request went to `api.anthropic.com`:

| Path | Default | `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` |
|---|---|---|
| `POST /v1/messages` | yes | yes |
| `GET /api/claude_code/settings`, `GET /api/claude_code/policy_limits` | yes | yes; a 403 is tolerated |
| `GET /api/claude_cli/bootstrap`, `POST /api/eval/sdk-*`, `POST /api/event_logging/v2/batch` | yes | no |
| `GET /mcp-registry/v0/servers` (no auth) | yes | no |

With nonessential traffic off and everything except `/v1/messages` refused, a prompt with a tool call worked normally. The model request policy therefore starts as: allow `POST /v1/messages`, deny the rest.

Prior art: Claude Code cloud environments (secrets injected by the egress proxy), Codex network secrets, Boundary's injected credentials, Zalando's agentic identity broker.

Milestone 1, 2026-10-26, in kind with the real runtime:

- `ext_authz`'s `CheckRequest` carries the session pod's IP as the source address, and the authorizer maps it to the session from a watch on session pods.
- Cilium prefixes the names of resources in a CiliumEnvoyConfig with `<namespace>/<config>/`, and rewrites only the references it knows. `ext_authz`'s `envoy_grpc.cluster_name` needs the prefixed name. Envoy then uses that name as the HTTP/2 `:authority`, and Rust's HTTP/2 server rejects the slashes, so `envoy_grpc.authority` has to be set as well.
- The authorizer trusts the source address in the request, and its answers carry credentials, so its port is restricted to Cilium's Envoy (the `host` and `remote-node` entities).
- git shows only the status of a refused push, not the response body. The authorizer therefore refuses pushes to other refs with a `200` in git's receive-pack format, which git prints as `remote:` output and `! [remote rejected]` lines.

2026-10-27: session DNS now matches egress, as decided above; until then the policy let sessions look up any name. Sessions may look up the hosts they may reach and names under the cluster domain. Cluster DNS answers those itself, and they cover the runtime's Service and the resolver's search-list expansions (`github.com.<ns>.svc.cluster.local`). Those expansions have to come back NXDOMAIN: glibc and musl both give up on a refused one without trying the name as given. Nodes that add their own search domains therefore need Cilium's `dnsProxy.dnsRejectResponseCode=nameError`.

2026-10-27: blocked egress fails fast ([#33](https://github.com/bo0tzz/sloppod/issues/33)). Silently dropped connections left clients hanging until their own timeouts, e.g. minutes of retries in `npx playwright install`. Blocked names now fail at lookup, and the resolver's error names the host. For the rest (allowed hosts on other ports, bare IPs), Cilium's `policyDenyResponse=icmp` answers with a Destination Unreachable, which session policies allow in. Envoy can't name the host for TLS connections to arbitrary hosts, because it has no certificate to present for them.

2026-10-27: GitHub tokens follow the session's grants ([#36](https://github.com/bo0tzz/sloppod/issues/36)). Until then, sessions got installation tokens with every repository and permission the App has, cached per installation and shared across sessions and users. A session could therefore read every private repository in its installation, and its push token could do anything the App can. Now:

- A push gets a token for the one granted repository, with `contents: write` (and `workflows: write`, see below).
- A read (git or API) through a granted repository gets a token for the granted repositories in that repository's installation, at read level only. An API path that names no granted repository uses the first granted one whose installation still mints a token; with none, the read goes unauthenticated. Read levels: `actions`, `checks`, `contents`, `issues`, `metadata`, `pull_requests` and `statuses`, as far as the App has them. It's a fixed list, because some permissions (`workflows`) have no read level.
- Tokens are cached per (installation, repositories, permissions). Sessions with the same grants share a token, and none gets one wider than its own grants.
- The runtime's own calls keep the installation-wide token, e.g. carrying out approved proposals or looking up the App's bot user. That token never reaches a session.

Reads beyond the grants stay allowed, but only of public data. git reads of other repositories go out without a token. API reads carry the session's read token, which reaches public data and the granted repositories and nothing else. Private repositories need a `repo` grant. The reasoning: the `github` capability is read access to GitHub ([ADR 2](0002-capabilities.md)), and public GitHub is part of the open web. Refusing it would stop agents reading their dependencies' source and issues, and wouldn't keep anything secret. What least privilege has to narrow is the credential, which was the thing that reached private data. The cost is that sessions without a repository grant read unauthenticated, under GitHub's limit for anonymous API calls: 60 an hour per IP, shared by the cluster's egress address.

2026-10-27: Envoy's upstream connections never resume TLS sessions. Every intercepted host shares one upstream TLS context, whose session cache isn't keyed by SNI. A connection to chatgpt.com resumed a session from another Cloudflare-hosted name, came back with that name's certificate and failed the SAN check. Every connection now does a full handshake and is verified against the host it's for.

2026-10-28: not all egress goes through the authorizer. A session with the `packages` capability reaches package registries and tool downloads (`SLOPPOD_DIRECT_HOSTS`, the chart's `session.directHosts`; crates.io, npm, PyPI, Go, GitHub releases, mise and others by default) directly on ports 80 and 443, allowed by Cilium's FQDN rules, with no listener redirect and no credential. These are public, unauthenticated downloads, and sending them through Envoy would gain nothing but latency. A host that needs a credential, or whose requests need deciding one by one, belongs to a request policy instead, and is then intercepted.

2026-10-28: the listener normalises request paths before the authorizer sees them (`normalize_path`, `merge_slashes`): dot segments are resolved and repeated slashes merged, so the authorizer and the upstream see the same canonical path. Until then Envoy passed paths as sent, and each request policy that matched a path prefix had to defend against `..` itself; two didn't ([#26](https://github.com/bo0tzz/sloppod/pull/26), [#172](https://github.com/bo0tzz/sloppod/pull/172)). Policies keep their own checks as a second layer. Escaped slashes (`%2F`) are left as sent.

2026-10-28: each session has its own Envoy listener, and the authorizer knows the session from it rather than from the request's source IP ([#196](https://github.com/bo0tzz/sloppod/issues/196)). A shared CiliumEnvoyConfig (`sloppod-egress`) holds the upstream and authorizer clusters and the listeners' certificate, served to them over SDS; each session's config (`sloppod-egress-<session>`) has one listener that refers to those by qualified name (`<namespace>/sloppod-egress/<name>`) and stamps the session's id on every authorization request (`ext_authz` `context_extensions`). The session's CiliumNetworkPolicy redirects its intercepted hosts to its own listener; Cilium picks the listener from the policy, which selects the pod by its session label, so a pod can't reach another session's. The authorizer refuses a request whose listener names no session. The cost is one listener and proxy port per running session: parking a session deletes its listener, and its policy then denies all egress, since the pod may run on for a while after it is told to stop; waking applies both again. Reading Cilium's own source identity in Envoy was tried first: cilium-envoy keeps it in filter state that formatters can't read, so it can't reach `ext_authz` without a change upstream.

2026-10-28: a push token also has `workflows: write` where the App's installation has it. GitHub refuses a push that changes `.github/workflows` without it, so until then sessions couldn't change their repository's CI at all and had to hand such changes to a human to push. That matches [ADR 3](0003-proposals-and-trusted-executor.md), which already treats a session's workflow changes as collaborator-level trust. The permission is asked for only when the installation has it, because GitHub refuses a token that asks for more than the installation has. An App without it keeps working, and its sessions' workflow pushes are refused as before.

2026-10-29: writes to a session's egress are ordered ([#345](https://github.com/bo0tzz/sloppod/issues/345)). Each grant change applied the session's policy again from a task of its own, with nothing ordering the tasks against each other or against parking. One that read the grants before a revocation, then stalled in the patch or a retry's backoff, could land after the revocation's, or after a park's release, and leave the pod direct egress it no longer had, until the next change. Now every write to a session's egress (placing, a grant change, parking, startup's release of parked sessions, removal) takes the session's egress lock, and an apply reads the session's grants and phase under it, every retry afresh. Whatever lands last therefore reflects the session as it is: a revocation takes effect once its own write lands, and once a session is parked no apply gives its pod egress again until it wakes.
