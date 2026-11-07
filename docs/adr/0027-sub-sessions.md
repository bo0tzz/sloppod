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

2026-10-28: a report can hand over a branch other than the session branch ([#257](https://github.com/bo0tzz/sloppod/issues/257)). A child reused for a later task can't push that task to its session branch, which still holds the first and can't be force-pushed, so `report_done` takes a `head`: a branch named after the session branch, by the same rule as a pull request's. That branch is compared and reviewed against the base as the session branch would be. A report also takes a `title` for the task, which defaults to the session's title and can be corrected between review rounds like the summary.

2026-10-28: a sub-session is told, in its prompt, its parent's id: that its chat replies reach only the user, that `message_session` reaches its parent and `report_done` hands its work back, and that its parent's messages direct its work but aren't the user's approval ([#290](https://github.com/bo0tzz/sloppod/issues/290)).

2026-11-03: "a child that has finished parks like any idle session" assumed ADR 25's idle parking, which isn't built, so finished children stayed idle and held their tree's slots until the user parked them by hand ([#284](https://github.com/bo0tzz/sloppod/issues/284)). Two things now free them. A parent can park its own children with `park_session {id}`, next to `message_session`; a parked child wakes on a message as before. And a child parks by itself once it has finished: a report of it was carried out, nothing of its is in review or waiting on findings it may answer, and it has been idle for 5 minutes with nothing queued and, as its supervisor says, none of its commands running. A child whose supervisor doesn't say what's running is left alone.

2026-11-03: a report is compared with the parent's branch it started from only while that branch has something of its own ([#373](https://github.com/bo0tzz/sloppod/issues/373)). A parent whose branch was behind main when it spawned the child made the review show main's commits as the child's change, and the child couldn't move its parent's branch or its report's base to fix that. When the parent's branch is behind the repository's default branch with nothing of its own, the default branch is the base instead: compared from its merge base with the child's branch, that shows the child's change alone. Pull requests, stacks and code reviews based on a session branch the work started from get the same.

2026-11-03: a child no longer inherits its parent's model by default ([#283](https://github.com/bo0tzz/sloppod/issues/283)). Much of what children do is well-specified work that a cheaper model handles fine, while the parent's managing, design and integration benefit from the bigger one, and on 2026-10-28 children used up the Claude usage limit twice. The user has a setting for sub-sessions next to their default model: a model, the parent's, or (the default) the agent's cheaper model where its adapter names one, Sonnet for Claude Code; Codex names none, so its children keep the parent's model. `spawn_session`'s `model` still overrides it, so a parent can give a hard task the bigger model. A child's model shows in `list_sessions`, in `spawn_session`'s answer and in the UI's session tree.

2026-11-05: a sub-session has no `highlight` tool ([#439](https://github.com/bo0tzz/sloppod/issues/439), ADR 4). It doesn't talk to the user; its report reaches its parent, which highlights what the user needs.

2026-11-07: inference grants are the exception to "at most its parent's grants" ([#461](https://github.com/bo0tzz/sloppod/issues/461)). A child can run on a model of one of the owner's Anthropic-compatible endpoints, named in `spawn_session`'s `model` or by the sub-session setting, and gets the `endpoint` grant that model needs whether or not its parent holds it; a parent's endpoint grant isn't copied, since each session's follows its own model. The intended use is a parent on a frontier model handing well-specified work to children on cheaper ones, and requiring the parent to hold a grant for every endpoint its children might use would mean main sessions carrying grants they don't use. Attenuation bounds what a tree can reach and change: repositories, hosts, identities. A model reaches nothing: it's where the agent's own requests go, with the owner's key, and what it costs is spend, which the tree limit bounds now and per-session budgets will (#461's spend). Write access and everything else stay at most the parent's.
