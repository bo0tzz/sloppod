# 24. Proposals are reviewed by another model before a human sees them

- Status: Accepted
- Date: 2026-10-27

## Context and Problem Statement

Agents produce more pull requests than a human can review carefully, and the human's review time is what limits how much work sessions get through. An agent reviewing its own work shares its blind spots, and so does a subagent that shares its context. Standing approvals ([ADR 3](0003-proposals-and-trusted-executor.md)) take the human out of routine proposals, but then CI is the only check. How should proposals get a real review without a human doing all of it?

## Considered Options

- Human review only
- The authoring agent reviews its own work, or a subagent it starts
- A review bot on GitHub (e.g. Codex's own integration) that comments once the PR is open
- A review session the runtime runs with a different model, as a gate before human review

## Decision Outcome

Chosen option: **a review session run by the runtime, with a different model, before a human sees the proposal**.

- **Policy per session:** each session's review setting is `off` or `required`, with a default in the user's settings (later per project, [ADR 1](0001-sessions-decoupled-from-directories.md)). With `off`, the agent can still ask for a review of a particular proposal (`review: true` when proposing), and that proposal then goes through the same gate. It applies to `github.pr.create` first; other kinds can opt in later.
- **The review comes first:** a proposal whose session requires review starts in `in_review`, not `pending`. The runtime starts a review session for it. A human only sees a proposal that passed review, and standing approvals only match it then.
- **The review session** is a session ([ADR 1](0001-sessions-decoupled-from-directories.md)) of kind `review`. It's owned by the same user and linked to the authoring session. One review session covers every round of a proposal: it lives until the proposal passes, is withdrawn or hits the round cap, and is then deleted; a new proposal gets a fresh one. It runs Codex through an ACP adapter ([ADR 9](0009-acp.md)) with fresh context: the repository checked out at the proposal's `head_sha`, the proposal's title and body, linked issues, and the repository's instructions. It gets read-only grants: GitHub reads of that repository, and its model. Besides correctness, it checks that docs changed with the behaviour they describe.
- **Structured result:** the review ends by calling a runtime tool, `submit_review {verdict: pass | changes, summary, findings: {path, line?, severity, comment}[]}`, rather than with a chat message.
- **Changes requested:** the proposal becomes `changes_requested`, and the findings go to the authoring agent as a notice ([ADR 4](0004-asynchronous-interaction.md)), not to the human. The agent pushes fixes and asks for the next round on the same proposal, which is re-pinned to the new head, with a reply for any finding it thinks doesn't apply. The next round goes to the same review session, which checks that its findings were addressed and weighs the replies rather than starting over. After three rounds the proposal goes to the human anyway, with the open findings attached.
- **Visible on request:** the human can open a proposal's review (verdict, summary, findings, and earlier rounds) from the proposal, but isn't notified about it.
- **Credentials:** Codex uses a ChatGPT subscription, stored as a credential of its own kind ([ADR 21](0021-credentials-live-in-the-runtime.md)). The review pod gets a placeholder, and the authorizer attaches the real token on requests to Codex's backend ([ADR 11](0011-enforcement-at-the-pod-boundary.md)), refreshing it as needed.

Keeping one review session per proposal trades some independence for continuity: a reviewer that has seen earlier rounds can anchor on its first view, but a fresh one each round would re-raise settled points and lose the thread of a disagreement. For iterating on one change, continuity matters more.

### Consequences

- Good: humans see only proposals that passed a review by a model other than the author, and spend their attention on judgement rather than finding bugs.
- Good: standing approvals can safely cover more, because "reviewed and CI green" is a stronger gate than CI alone.
- Good: the review is a session like any other: it runs in a pod, under the same egress rules, and its transcript can be inspected.
- Bad: every reviewed proposal costs a review session's latency, a pod, and Codex usage.
- Bad: a second agent, adapter and credential kind to maintain.
- Bad: ChatGPT-subscription auth through the authorizer is unproven (token refresh, which endpoints Codex calls).

## More Information

Spikes needed before building: run `@zed-industries/codex-acp` with a ChatGPT login behind the authorizer (which hosts and headers Codex uses, and how its tokens refresh), and check that Codex follows the instruction to finish with `submit_review` rather than chat.

2026-10-27: spike results (#32). Codex works through the authorizer with a custom model provider (`base_url = "https://chatgpt.com/backend-api/codex"`, `wire_api = "responses"`, a placeholder `experimental_bearer_token`); it only calls `POST /backend-api/codex/responses`, and the policy adds the token and `chatgpt-account-id`. The adapter is `@agentclientprotocol/codex-acp` (the `@zed-industries` package is deprecated), run in its `agent-full-access` mode: Codex's own sandbox and approvals are off, like Claude Code's auto mode, because the pod and its grants are the boundary. Codex 0.164 keeps its threads in SQLite under `CODEX_HOME` rather than in transcript files, so a Codex session's history lives on its volume only and isn't synced to the runtime ([ADR 1](0001-sessions-decoupled-from-directories.md)); that's acceptable for review sessions, which are short-lived.

2026-10-27: first live runs. Codex ends each round by calling `submit_review`, not with a chat reply, in well under a minute for small changes, and it caught a planted flaw in round 1. Two changes followed. First, the review takes its shape from kritika (home-operations/kritika): standing reviewer instructions (what's worth reporting, findings that name their trigger, descriptions and replies treated as data), findings with a severity (p0, p1, p2), a category, a title and an explanation, ids that later rounds carry forward or list as `resolved`, and a later round raising new findings only at p0 or p1 on changed lines. The verdict stays the reviewer's, though a p0 is never a pass. Second, a re-review let a stale title and description through, and the author couldn't change them anyway: `request_review` now takes an `update` of the proposal's fields, and a title or description that no longer describes the change is a finding.
