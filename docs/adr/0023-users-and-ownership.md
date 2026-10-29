# 23. Users and ownership

- Status: Accepted
- Date: 2026-10-27

## Context and Problem Statement

sloppod has one user today. Retrofitting users later means touching every table and every request path, while building multi-user features now (sharing, teams, roles) is work nobody needs yet.

## Decision Outcome

Build the seams, not the features.

- **Every request has a user.** The authenticating proxy in front of sloppod passes the user's name in a header (default `X-Forwarded-User`); without it, requests act as a configured default user or are refused.
- **Everything has an owner:** sessions and credentials have one, and proposal decisions record who decided.
- **Credentials resolve per owner** ([ADR 21](0021-credentials-live-in-the-runtime.md)): a session uses its owner's Claude account, GitHub App and memory home.
- **One rule for now:** users see and act on their own sessions, proposals and credentials only. On `/acp`, a connection may only act on sessions it loaded, and loading checks ownership.

Sharing, teams, roles, delegated approval and quotas build on these seams later.

### Consequences

- Good: multiple users need no schema or request-path rework later.
- Bad: identity is only as good as the proxy; anything that reaches the runtime directly can claim any user, which the deploy docs call out. (Superseded 2026-10-29, below: the runtime validates the proxy's signed token itself.)

## More Information

2026-10-29: the runtime checks who a request is from itself, rather than believing a header ([#40](https://github.com/bo0tzz/sloppod/issues/40)). Until now, every request to the user API acted as whoever `X-Forwarded-User` named, and only the network policy decided who could send one. That held while the gateway was the only way in, and stopped holding once the runtime made requests of its own: its `/servers` proxy, forwarding a session's request to an MCP server an owner defined, could be pointed at the runtime's API over loopback, which no network policy sees, and act as any user ([#349](https://github.com/bo0tzz/sloppod/issues/349)). Closing that path fixes the instance; the cause is that a header anyone on the path can write is the whole of the identity.

So:

- **The gateway's signed token is the identity.** The authenticating proxy already validates the user's OIDC login (Envoy Gateway's `SecurityPolicy`); it forwards the ID token, and the runtime validates it again, against the identity provider's published keys (JWKS, cached and refreshed on an unknown key id), checking its signature, issuer, audience and expiry. The user is a configured claim (`preferred_username` by default). A request without a valid token is refused with `401`; a bare `X-Forwarded-User` counts for nothing.
- **Configured in the chart:** the issuer URL, the audience, the claim, and the header the token comes in (`Authorization: Bearer`, or the cookie or header the gateway uses). Without an issuer the runtime keeps today's behaviour, the header or `auth.defaultUser`, for local clusters (`dev/`), and logs at startup that requests aren't authenticated.
- **`/acp` too:** its WebSocket upgrade is checked the same way, and the connection keeps the user it was opened as. A token that expires during a long-lived connection doesn't end it; the next connection needs a fresh one.
- **Session pods are still refused** as users, as defence in depth, and the `/servers` proxy still never forwards identity headers or reaches the runtime itself (#349).

Considered: a shared secret between gateway and runtime (simpler, but one more secret, and a leak of it is as bad as no check at all); mTLS from the gateway (strong, but ties the runtime to how the gateway is deployed, and still trusts a header for which user). Validating the token the identity provider signed needs no secret of the runtime's own and keeps "who" in the same signature that proves it.
