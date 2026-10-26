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
- **Failure and cost:** the authorizer is a small replicated component, separate from the main runtime process, and fails closed (`failure_mode_allow: false`). Body inspection is opt-in per service: on for GitHub, off for the model API so large prompts aren't buffered.
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

Prior art: Claude Code cloud environments (secrets injected by the egress proxy), Codex network secrets, Boundary's injected credentials, Zalando's agentic identity broker.
