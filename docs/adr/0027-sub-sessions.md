# 27. Sessions fan out work to sub-sessions

- Status: Proposed
- Date: 2026-10-28

## Context and Problem Statement

Much of the work a session takes on parallelizes: independent fixes, a change and its docs, a spike beside the main line. In-session subagents help with small pieces, but they share the parent's pod (its CPU and memory), die with it on every deploy, and act with all of its grants. ADRs 1, 2 and 4 already sketch the answer: a session spawns a linked sub-session with its own pod and branch, starting with at most a subset of its grants, and the two exchange messages delivered at the next tool boundary ([#30](https://github.com/bo0tzz/sloppod/issues/30)).

How does a session spawn sub-sessions, talk to them, and get their work back?

## Decision Outcome

**A sub-session is an ordinary session with a parent.**

- **Spawning.** A session calls `spawn_session {task, repo?, grants?, agent?, model?}`. The child is a session of its own, with its own pod, volume and agent, linked to its parent (`parent_id`), and `task` is its first message. Its grants are at most its parent's (attenuation, [ADR 2](0002-capabilities.md)): those given, or all of the parent's but its proposal standing approvals. Anything more, it requests like any session.
- **Its branch is named after its parent's**, `<parent branch>-<child name>`, and starts from the parent's branch as it is when the child is spawned. The authorizer already lets a session push to branches named after its own, so the parent can rebase, merge or propose the child's branch itself, e.g. as a layer of a stack. Nothing new is needed for approval: only people approve ([ADR 3](0003-proposals-and-trusted-executor.md)), and what they see is the parent's proposal.
- **Messages.** `message_session {id, text}` sends a message to the session's parent or one of its children, delivered as a notice at the next tool boundary ([ADR 4](0004-asynchronous-interaction.md)). When a child's turn ends, its parent gets the child's last message: the report of what it did, with no special call to remember.
- **Seeing them.** `list_sessions` gives a session its children: their state, branch and last message. In the UI, children are listed under their parent, and the user can open and talk to any of them.
- **Lifecycle.** A child that has finished parks like any idle session. Stopping or deleting a session stops its children.
- **Limits.** A session has at most 5 children running at once, so a runaway fan-out can't take the cluster or the owner's quota.

### Open points

- **Whose proposals:** whether a child proposes to the user directly, or only reports back for its parent to propose. Back to the parent keeps one stream to review, with the parent integrating; direct proposals suit independent tasks.
- **When the parent hears:** at each of a child's turn ends (above), or only when the child says it's done, which is quieter but can be forgotten.
- **Review policy:** whether children inherit their parent's (review by Codex first, [ADR 24](0024-automated-review-before-human-review.md)).

### Consequences

- Good: work fans out across pods, each with only the grants it needs, and survives deploys like any session.
- Good: no new approval path: the parent's branch rules and proposals carry the children's work.
- Bad: more pods and more of the owner's quota at once; the limit bounds it.
- Bad: the parent's context holds its children's reports; long fan-outs need the parent to summarize.

## More Information

Considered and not chosen:

- **In-session subagents only:** they share the parent's pod and grants and die with it.
- **Children approving each other's proposals, or a parent approving its child's:** ADR 2 has only people approve; changing that is a separate decision.
