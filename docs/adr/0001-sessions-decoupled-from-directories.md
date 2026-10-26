# 1. Sessions are decoupled from directories and owned by the runtime

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Claude Code stores a session under a directory derived from its working path. Moving or renaming a project breaks resume. Sessions started in a temporary directory, such as an AUR build or a removed worktree, become orphaned: in one local history, 138 of 766 sessions have a working directory that no longer exists.

Tying a session to a directory also doesn't match how sessions are used:

- Most stay in one repository.
- Some start with no particular scope and narrow later.
- Some cover a group of repositories, e.g. an org checkout with several sibling repos.
- Occasionally a session needs a small change in another repository alongside its own.

## Decision Outcome

A session is the runtime's own object. It has a stable ID, a name, a transcript and a set of grants (see [ADR 2](0002-capabilities.md)), and it is independent of any filesystem path.

- **What a session works on is just its grants**; there is no separate notion of scope.
  - Access to a repository is a capability, materialized as a checkout inside the session's pod. The checkout lives on a session branch, so the session can be moved between pods.
  - A session can start with no repository grants and gain them later. A session spanning an org holds a grant like `repo:immich-app/*:ro`.
  - UI grouping and memory namespaces are derived from a session's repository grants.
- **Transcripts are owned by the runtime.** It keeps its own event log as the record used for the UI, search and replay, plus the agent's native transcript as an opaque blob. When a session is placed in a pod, the runtime restores that blob where the agent expects it. For Claude Code this means `CLAUDE_CONFIG_DIR` plus `CLAUDE_CODE_PROJECT_DIR_NAME=<session id>`.
- **Writes to another repository use a sub-session.** A session that needs to change another repository spawns a linked sub-session, and the two exchange messages. The sub-session starts with at most a subset of its parent's grants, and requests anything else, such as write access to that repository, like any other session ([ADR 2](0002-capabilities.md)).
- **Read-only reference clones** of other repositories are a separate, cheap capability. They are the common case: 261 of 766 sessions only read other repositories.

### Consequences

- Good: sessions survive moving, renaming and deleting directories, and survive pod restarts.
- Good: a session can gain or drop repositories without losing history.
- Bad: two copies of every transcript have to be kept in sync. The agent's native format is internal to the agent and changes between versions, so the blob must be treated as opaque.

## More Information

Spike, 2026-10-26: restoring only the single `<session>.jsonl` into a fresh `CLAUDE_CONFIG_DIR` and resuming from a different working directory recovered the conversation. Without a pinned `CLAUDE_CODE_PROJECT_DIR_NAME`, Claude Code also resumes, but writes into a new directory named after the path.

Prior art: Coder Agents, OpenHands and Claude Code cloud sessions all keep the conversation in the control plane and treat the sandbox as disposable.
