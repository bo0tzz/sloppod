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
- **Whose account:** the owner of what's being judged: their default inference account ([ADR 20](0020-switchable-inference-accounts.md), [ADR 21](0021-credentials-live-in-the-runtime.md)). Like a session's own calls, these go on their quota and bill. A user with no account gets no judgements, and the features that need them do without.
- **Which model:** a small, fast one by default (Haiku), configurable. Judgements are frequent and simple.
- **Advisory only.** A judgement can make sloppod tell someone something: a notice to an agent, a hint in the UI, a suggested state change. It never authorizes anything. Capabilities, proposals and enforcement ([ADR 2](0002-capabilities.md), [3](0003-proposals-and-trusted-executor.md), [11](0011-enforcement-at-the-pod-boundary.md)) don't consult it. The content it reads often comes from an agent or the internet, so a judgement can be steered by what it reads, and the worst it can do is send a misleading notice.
- **Structured replies.** Each purpose declares the JSON its reply must match; a reply that doesn't parse is treated as no judgement.
- **Bounded and visible.** Calls are rate limited per owner and per purpose, logged with their purpose and outcome, and counted, so a user can see what sloppod spent on their behalf.

The first use is the turn-end check: when a turn ends, the runtime asks whether its last message says work is starting that nothing is doing. "Nothing is doing" is what the supervisor reports about the commands running under the agent ([ADR 17](0017-in-pod-supervisor.md)), so the check is the same for every agent. If the answer is yes, the session gets a notice naming what it said it would do.

### Consequences

- Good: judgement that doesn't depend on the agent's harness, the same for every agent ([ADR 9](0009-acp.md)), made outside the session it's about.
- Good: one place for every such call, with its limits, logging and accounting.
- Bad: the runtime spends the owner's quota on its own; small calls, but every turn end can cost one.
- Bad: a dependency on the inference provider for features that didn't have one. Each degrades to doing nothing when calls fail.
- Neutral: subscription accounts are meant for their own clients. Whether the runtime may call the API with one directly, and what that requires, needs a spike before anything else is built on it.

## More Information

Considered and not chosen:

- **Agent-harness hooks** (a Claude Code `Stop` hook judged by a small model): available today, and close to what the turn-end check needs, since Claude Code passes the hook the last message and the background tasks it knows of. But it's per harness, so Codex sessions would go without, and each new adapter would need its own.
- **Heuristics** (matching "I'll", "next", "starting"): cheap, but too blunt for prose, and every use would need its own rules.
- **Asking the session's own agent**: costs a turn at the session's model, and the agent judging itself is what the turn-end check is meant to catch.
