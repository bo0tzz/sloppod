# 27. Sessions fan out work to sub-sessions

- Status: Proposed
- Date: 2026-10-28

## Context and Problem Statement

Much of the work a session takes on parallelizes: independent fixes, a change and its docs, a spike beside the main line. In-session subagents help with small pieces, but they share the parent's pod (its CPU and memory), die with it on every deploy, and act with all of its grants. ADRs 1, 2 and 4 already sketch the answer: a session spawns a linked sub-session with its own pod and branch, starting with at most a subset of its grants, and the two exchange messages delivered at the next tool boundary ([#30](https://github.com/bo0tzz/sloppod/issues/30)).

How does a session spawn sub-sessions, talk to them, and get their work back?

## Decision Outcome

**A sub-session is an ordinary session with a parent.**

- **Spawning.** A session calls `spawn_session {task, repo?, grants?, agent?, model?}`. The child is a session of its own, with its own pod, volume and agent, linked to its parent (`parent_id`), and `task` is its first message. Its grants are at most its parent's (attenuation, [ADR 2](0002-capabilities.md)): those given, or all of the parent's but its proposal standing approvals. Anything more, it requests like any session, but for write access to a repository (below).
- **It works in a repository its parent can push to**, on a branch named after the parent's there, `<parent branch>-<child name>`, starting from the parent's branch as it is when the child is spawned. Other repositories of the parent's it can only read. A child can't get write access to a repository its parent can't push to: it reports that it needs it, and the parent requests the repository ([ADR 1](0001-sessions-decoupled-from-directories.md)) and spawns a child for it. So every child's branch is one its parent can integrate, and no child widens what its tree can change. The authorizer already lets a session push to branches named after its own, so the parent can rebase, merge or propose the child's branch itself, e.g. as a layer of a stack. Nothing new is needed for approval: only people approve ([ADR 3](0003-proposals-and-trusted-executor.md)), and what they see is the parent's proposal.
- **Messages.** `message_session {id, text}` sends a message to the session's parent or one of its children, delivered as a notice at the next tool boundary ([ADR 4](0004-asynchronous-interaction.md)).
- **Reporting back.** A child's work goes back to its parent, not to the user: the parent integrates it and proposes it. A child says it's done with `report_done {summary}`. If its review policy requires review, its branch is reviewed first, in rounds like a pull request proposal ([ADR 24](0024-automated-review-before-human-review.md)): findings go back to the child, and the parent hears once the review is over, with the branch, the summary and how the review ended (see the note below). There's no human approval in this, as nothing outward happens: the parent's own proposal of the work is reviewed and approved as usual. The parent still does its own pass while integrating.
- **Unsticking.** A child whose turn ends without reporting, with none of its commands running, is checked by a judgement ([ADR 26](0026-runtime-inference.md)): if it stopped short of its task, it's nudged to carry on or report.
- **Review policy** is inherited from the parent.
- **Seeing them.** `list_sessions` gives a session its children: their state, branch and last message. In the UI, children are listed under their parent, and the user can open and talk to any of them.
- **Lifecycle.** A child that has finished parks like any idle session. Stopping or deleting a session stops its children.
- **Limits.** Children can spawn children, but a tree of sessions, its root and all of its descendants, has at most 5 sub-sessions running at once. Spawning one, or waking a parked one with a message, is refused while the tree is at the limit. So a runaway fan-out, wide or deep, can't take the cluster or the owner's quota.

### Consequences

- Good: work fans out across pods, each with only the grants it needs, and survives deploys like any session.
- Good: no new approval path: the parent's branch rules and proposals carry the children's work.
- Bad: more pods and more of the owner's quota at once; the limit bounds it per tree.
- Bad: work in a repository the parent can't push to takes a request by the parent and a new child, not just the child's own request.
- Bad: the parent's context holds its children's reports; long fan-outs need the parent to summarize.
- Bad: a child's work is reviewed twice, as itself before its report and as part of the parent's proposal; the second covers the integration, which the first can't.

## More Information

Considered and not chosen:

- **In-session subagents only:** they share the parent's pod and grants and die with it.
- **Children approving each other's proposals, or a parent approving its child's:** ADR 2 has only people approve; changing that is a separate decision.

2026-10-28: the report is a proposal (`sloppod.session.report`), so it's reviewed in rounds like any other, and proposal kinds now say whether they're outward. One that isn't is carried out by the runtime once its review is over, not by a person. The parent hears once the review is over, not only when it passes: a review still with findings after the last round, or one that stalled, would otherwise leave the child's work with nobody, so the parent gets it with the findings attached, or marked unreviewed, and judges it in its own pass. Standing approvals still match only outward proposals that passed review.
