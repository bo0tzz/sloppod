# 20. Inference accounts can be switched under a running session

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

A session's agent uses a model account: a subscription or an API key. We want to change which account a running session uses without disturbing it. Reasons include:

- an account hitting its usage limits
- separating work and personal use
- rotating or revoking an account

## Decision Outcome

The account is chosen by the runtime for each request, so switching is invisible to the agent.

- **It falls out of credential injection.** The agent only ever holds a placeholder, and the authorizer attaches the model credential to each request ([ADR 11](0011-enforcement-at-the-pod-boundary.md)). Pointing a session at another account changes what the authorizer attaches to the next request. There's no restart, and the agent doesn't notice.
- **Choosing an account:** the session's `model:<provider>` grant ([ADR 2](0002-capabilities.md)) names a set of accounts and a policy:
  - an explicit choice by the human
  - a per-project default, e.g. the work account for work projects
  - a failover order
- **Failover:** model API traffic uses Envoy's `ext_proc` filter instead of only `ext_authz`, so the runtime also sees response headers. When an account returns a rate-limit or usage-limit response, the runtime switches the session to the next account in its policy. The agent's own retry then goes out on the new account.
- **Usage accounting:** the same response path gives per-session and per-project usage, from rate-limit headers and, optionally, the usage figures in response bodies.
- **Limited to one provider.** Switching between accounts of the same provider is seamless. Changing provider (e.g. Anthropic to Bedrock) changes the agent's own configuration, so it's a restart ([ADR 6](0006-self-reference.md)) that keeps the same transcript.

### Consequences

- Good: hitting a usage limit doesn't stop work, and which account pays for what is a policy, not a per-session setup.
- Bad: the provider's prompt cache is per account, so the first request after a switch is uncached: slower and more expensive.
- Bad: anything the agent CLI loads from the account at startup can be stale after a switch until the next restart. For Claude Code that includes the plan tier, organization policy limits, remote settings and model access.

## More Information

Untested: whether Claude Code tolerates the account behind its injected token changing mid-session. The spike: run a session through the authorizer, switch the injected token between two accounts between turns, and check that the conversation continues and what startup-derived state goes stale.

Cilium's Envoy build includes `envoy.filters.http.ext_proc` as well as `ext_authz`, so the response path needs no other proxy.

2026-10-28: usage is first tracked per credential without the response path ([#140](https://github.com/bo0tzz/sloppod/issues/140)). The authorizer still only sees requests, so the limits come from where they're reported instead. Claude Code reports a subscription's rate-limit windows in its `usage_update`s, which the runtime keeps under the session owner's default Claude credential. For a ChatGPT login, codex-acp reports nothing, so the runtime reads ChatGPT's usage endpoint itself every five minutes, with the login it holds. Each credential's latest windows are kept in the database and shown as meters in the UI's sidebar. Failover can build on the same record once the response path exists; per-session and per-project usage still needs it.

2026-10-29: accounts are chosen per grant ([ADR 29](0029-grants-name-their-credential.md), [#276](https://github.com/bo0tzz/sloppod/issues/276)). A session's `model` (or `codex`) grant names an ordered list of the owner's accounts, filled with all of them, the default first, when it's made, and the authorizer attaches the first. The owner switches a running session by putting another account first in its grants list, and its next request goes out on that account, as above; usage limits and the waits for them follow the account in use. Changing the default no longer moves running sessions.
