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
- Bad: identity is only as good as the proxy; anything that reaches the runtime directly can claim any user, which the deploy docs call out.
