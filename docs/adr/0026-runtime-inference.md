# 26. The runtime makes small model calls of its own

- Status: Proposed
- Date: 2026-10-28

## Context and Problem Statement

Some of what sloppod should do needs judgement that only a model gives, and doesn't belong to any session:

- **Noticing when an agent stops on a promise.** An agent working on its own often ends a turn on "I'm starting on X now" with nothing running, and then nothing happens until someone writes. Seeing that means reading the turn's last message.
- **Lifecycle decisions** ([ADR 25](0025-session-lifecycle.md)): whether a session that went quiet is done, or waiting on something it didn't say.
- **Summaries for people:** what a long-running session did while the user was away, or what a shutdown report ([ADR 17](0017-in-pod-supervisor.md)) means.
- **Classifying and checking content:** triaging issues, describing a pasted image ([#216](https://github.com/bo0tzz/sloppod/issues/216)).

An agent can do some of this for itself, through its harness: Claude Code has hooks that a small model evaluates. But that only covers Claude Code sessions, every agent adapter would need its own, and the decision would sit inside the session it's about. A session's own agent can't be asked either: every question costs a turn, at the session's model and price.

How should sloppod get this judgement?

## Decision Outcome

**The runtime calls models itself, for small, advisory judgements, on the owner's account.**

- **A runtime service, `inference`.** It's one place that makes these calls: a purpose (what's being asked and why, for logs), a prompt, and a reply schema. Every feature that needs a judgement uses it, rather than each calling a model in its own way.
- **Whose account, and which client:** the owner's subscription, through the real client. A Claude subscription is for Claude Code, so a judgement is a one-shot `claude -p` with a small model, run by the supervisor in the session's own pod: every session image ships Claude Code, whichever agent the session runs, and its model calls already go through the authorizer with the owner's account ([ADR 11](0011-enforcement-at-the-pod-boundary.md), [ADR 21](0021-credentials-live-in-the-runtime.md)). The runtime asks over the link, and the supervisor runs it apart from the agent, so it costs no turn and the agent doesn't see it. The runtime doesn't call the API itself: a subscription used outside its own client is refused (see More Information).
- **Which model:** a small, fast one by default (Haiku), configurable. Judgements are frequent and simple.
- **Advisory only.** A judgement can make sloppod tell someone something: a notice to an agent, a hint in the UI, a suggested state change. It never authorizes anything. Capabilities, proposals and enforcement ([ADR 2](0002-capabilities.md), [3](0003-proposals-and-trusted-executor.md), [11](0011-enforcement-at-the-pod-boundary.md)) don't consult it. The content it reads often comes from an agent or the internet, so a judgement can be steered by what it reads, and the worst it can do is send a misleading notice.
- **Structured replies.** Each purpose declares the JSON its reply must match; a reply that doesn't parse is treated as no judgement.
- **About a session, in its pod.** A judgement runs in the pod of the session it's about, so a session without a running pod gets none until it has one. Judgements about no session in particular (triage, say) would need a pod of their own: a small utility session per owner, decided when the first such feature comes.
- **Bounded and visible.** Calls are rate limited per owner and per purpose, logged with their purpose and outcome, and counted, so a user can see what sloppod spent on their behalf.

The first use is the turn-end check: when a turn ends, the runtime asks whether its last message says work is starting that nothing is doing. "Nothing is doing" is what the supervisor reports about the commands running under the agent ([ADR 17](0017-in-pod-supervisor.md)), so the check is the same for every agent. If the answer is yes, the session gets a notice naming what it said it would do.

### Consequences

- Good: judgement that doesn't depend on the agent's harness, the same for every agent ([ADR 9](0009-acp.md)), made outside the session it's about.
- Good: one place for every such call, with its limits, logging and accounting.
- Bad: the runtime spends the owner's quota on its own; small calls, but every turn end can cost one.
- Bad: a dependency on the inference provider for features that didn't have one. Each degrades to doing nothing when calls fail.
- Bad: judgements share the session pod's resources and its owner's quota, and a judgement waits for a `claude -p` start (a few seconds); fine for checks at a turn's end, not for anything interactive.

## More Information

2026-10-28, spike: a plain `POST /v1/messages` with a Claude subscription's OAuth token, made through the authorizer exactly as a session's calls are but without Claude Code's own request shape, is refused (`rate_limit_error`), while the same token works for Claude Code. Shaping the request to pass as Claude Code would be getting around the provider's rule for the subscription, not a fix, so it isn't an option.

Considered and not chosen:

- **An API key** credential, with the runtime calling the API itself: simplest, but users with only a subscription (the common case) would need to get one, billed separately.
- **A utility session per owner** (Claude Code in its own pod, like a review session, [ADR 24](0024-automated-review-before-human-review.md)), asked over ACP: holds a pod per owner, and each judgement is a whole agent turn. Kept for judgements that aren't about a session.

- **Agent-harness hooks** (a Claude Code `Stop` hook judged by a small model): available today, and close to what the turn-end check needs, since Claude Code passes the hook the last message and the background tasks it knows of. But it's per harness, so Codex sessions would go without, and each new adapter would need its own.
- **Heuristics** (matching "I'll", "next", "starting"): cheap, but too blunt for prose, and every use would need its own rules.
- **Asking the session's own agent**: costs a turn at the session's model, and the agent judging itself is what the turn-end check is meant to catch.
