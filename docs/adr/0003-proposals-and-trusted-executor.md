# 3. Outward actions are proposals executed by the runtime

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Some actions are visible outside the session and hard to undo:

- opening a PR
- posting a comment
- applying to a production cluster
- changing runtime configuration

Today they are gated by instructions ("show me the full PR before opening it"), which the agent can ignore or misread. We want enforcement, with a review step that shows the actual payload. We also want to be able to relax it for a session, a target or a period, the way auto mode relaxes tool approvals.

## Decision Outcome

An agent never performs an outward action itself. It submits a **proposal**: a structured object of a known kind with a payload, for example `github.pr.create`, `github.comment`, `kube.apply`, `runtime.config.change`. When the proposal is approved, a trusted **executor** in the runtime performs it immediately, using credentials the session never has. The agent is not told "you may now do X"; it is told what happened, e.g. the URL of the PR that was opened.

- **Proposal kinds** are an extension point ([ADR 7](0007-extension-points.md)). Each defines:
  - a schema
  - a renderer, which shows what the change looks like: the diff, the title and body, `kubectl diff` output
  - an executor
- **Review:** the human approves, edits fields, rejects, or replies to the agent.
- **Approvals are durable records,** not a blocked process. Submitting returns a pending ID and the agent carries on ([ADR 4](0004-asynchronous-interaction.md)). The outcome arrives later as a message.
- **Provenance:** only an explicit human action in the UI counts as approval. Messages, wake triggers and payloads from other sessions never do.
- **Relaxing the review:**
  - A **standing approval** auto-approves proposals that match a pattern (kind, target, which sessions, TTL), e.g. "PRs on `bo0tzz/sloppod` until Friday". The executor still performs them, so there is only ever one path for outward actions.
  - An optional reviewer model in the style of auto mode can auto-approve proposals that clearly match the session's stated intent, and escalate the rest. It is a runtime component that a human enables, not a session, and sessions have no way to reach it ([ADR 2](0002-capabilities.md): only humans approve).
- **Submission:** agents submit proposals with an explicit tool from the runtime. The per-request authorizer ([ADR 11](0011-enforcement-at-the-pod-boundary.md)) rejects direct attempts at an outward action, such as `POST /repos/{owner}/{repo}/pulls` from `gh pr create`, with a 403. The 403 says to use the propose tool, so CLI habits redirect themselves.
- **Repositories:** credentials attached to a session's requests are read-only, except pushes to the session's own branches. The authorizer enforces this by inspecting the ref updates in git's `receive-pack` request body, which an HTTP path rule can't see. Opening a PR, merging, tagging and pushing to other branches are proposals.
  - PRs are opened from the session branch, so the agent can keep iterating on review feedback.
  - Merging stays a human decision, so later pushes never reach the default branch unreviewed.
- **CI on session branches:** pushes trigger workflows like any collaborator's push, including workflow files the agent changed. We treat that as collaborator-level trust rather than adding machinery such as forks or blocking workflow edits. High-value secrets belong in GitHub environments restricted to protected branches. If needed later, the authorizer can ask before accepting a push that touches workflow files.

### Consequences

- Good: the PR sign-off rule, and rules like it, are enforced rather than requested.
- Good: one mechanism covers PRs, comments, cluster changes and changes to sloppod's own config.
- Bad: every outward action needs a proposal kind with a renderer and an executor.

## More Information

Spike, 2026-10-26: a denial from the authorizer, with its explanation in the body, was shown to the client as-is (`sloppod: graphql mutation denied. Writes go through proposals: use the propose tool.`, HTTP 403).

Prior art:

- The Copilot coding agent pushes only to `copilot/*` branches and opens draft PRs; the person who asked for the work can't approve them.
- The "Create PR" button in Claude Code cloud sessions creates the PR outside the agent.
- Coder's `propose_plan`.
- Atlantis plans terraform changes for review and applies them only after approval.

2026-10-27: two relaxations are built. Approving a PR can turn on GitHub's auto-merge, so "merging stays a human decision" now means the human decides at approval: auto-merge only merges the commit that was reviewed, and new commits pushed to its branch turn it off. Standing approvals match a proposal's kind, its target (the repository, for every built-in kind), optionally one session, and an expiry; they can only add the auto-merge edit. A matching proposal is approved as it's made and goes through the same executor, and its record names the standing approval that decided it.

2026-10-28: auto-merge survives a rebase ([#106](https://github.com/bo0tzz/sloppod/issues/106)). "Only merges the commit that was reviewed" is now "only merges the change that was reviewed": when a PR is opened with auto-merge, a fingerprint of its diff against its base is recorded, like Gerrit's trivial-rebase rule and `git patch-id` (the files and the added and removed lines, without positions, index lines or context). A push to its head still turns auto-merge off at once; once the push has landed, a matching fingerprint turns it back on, pinned to the new head, and any other change (a resolved conflict included, which is unreviewed code) leaves it off. A diff touching binary files gets no fingerprint, since its content isn't in the diff. Stacks keep the old rule.

2026-10-27: standing approvals now cover one session each, set from that session's menu ("Approve its pull requests automatically"); ones covering all of an owner's sessions were revoked. How much a session may do unattended is decided per session, alongside its review policy ([ADR 24](0024-automated-review-before-human-review.md)), under which a proposal only reaches a standing approval after it passes review.

2026-10-29: what a proposal puts on GitHub is recognised by the proposal, not by what it says ([#346](https://github.com/bo0tzz/sloppod/issues/346)). A proposal carried out again (a restart before its outcome was recorded) used to skip an issue whose title matched any issue filed in the last day, by anyone, and a comment whose text matched any comment already there. So someone else's issue with the same title swallowed the proposal's, reported as filed, and a replay after a day filed a duplicate. Now each issue and comment a proposal posts carries a hidden marker naming the proposal and the item's position (`<!-- sloppod:proposal/<id>/<n> -->`), and a replay looks for that: among issues updated since an hour before the proposal was made, or the target's comments. Executors are given the proposal's id for this.
