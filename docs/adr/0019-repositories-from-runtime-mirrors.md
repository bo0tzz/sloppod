# 19. Repositories come from runtime-managed mirrors

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Sessions work in git checkouts. A fresh full clone from GitHub for every session is slow for large repositories, repeats the same download over and over, and counts against rate limits. Checkouts also have to survive restarts and moves between nodes ([ADR 10](0010-sessions-run-in-pods.md)). Access to a repository is a grant that can change mid-session ([ADR 2](0002-capabilities.md)).

## Considered Options

- A fresh clone from the remote for every session
- A shared mirror volume mounted read-only into session pods, with clones using git alternates
- Mirrors kept by the runtime and served over git's smart HTTP protocol, through the authorizer

## Decision Outcome

Chosen option: **mirrors served by the runtime**.

- **Mirrors:** the runtime keeps a bare mirror of each repository that any grant refers to. It works for any git remote, not just GitHub. Mirrors are refreshed on a schedule, on webhooks where available, and on demand: a session's fetch can trigger an upstream refresh first, debounced.
- **Clones:** a session's checkout is a partial clone (`--filter=blob:none`) from the runtime's git endpoint. Blobs are fetched on demand from the mirror, so even large repositories start quickly and use little storage per session. The checkout lives on the session's volume, on the session branch.
- **Fetch from the mirror, push to the real remote:** `remote.origin.url` points at the mirror and `remote.origin.pushurl` at the real remote. Pushes go through the authorizer, which enforces session-branch-only pushes ([ADR 3](0003-proposals-and-trusted-executor.md)).
- **Access is per request:** reads from the mirror go through the same authorizer ([ADR 11](0011-enforcement-at-the-pod-boundary.md)), so a grant added or revoked mid-session applies immediately, without remounting anything. Read-only reference clones ([ADR 1](0001-sessions-decoupled-from-directories.md)) are the same mechanism with no session branch.
- **Workdir volume:** each session has its own PVC holding its checkout and working tree (e.g. through agent-sandbox `volumeClaimTemplates`, [ADR 16](0016-agent-sandbox.md)). Uncommitted work survives restarts, preemption and moves: the volume detaches and reattaches wherever the next pod runs. After a node crash, a block volume stays attached to the dead node until Kubernetes declares it gone, which takes minutes unless the node is fenced.
- **Work-in-progress snapshots** are the fallback for when the volume itself is unavailable or lost. At the end of each turn and on shutdown, the supervisor snapshots the working tree without touching the index or `HEAD` (as `git stash create` does). It pushes the snapshot to a hidden ref on the runtime's mirror, `refs/sloppod/wip/<session>`, never upstream, so half-done work never triggers CI or clutters the session branch. A replacement checkout is rebuilt from the mirror, the session branch and the latest snapshot.
- **Submodules and LFS:** submodules resolve through the same mirrors, and each submodule repository needs its own grant. LFS objects go through the authorizer to the real remote.

### Consequences

- Good: each repository is downloaded from upstream once. Session starts are fast, and storage per session is small.
- Good: access follows grants request by request, with no shared writable state between sessions, and the mirror is free to garbage-collect.
- Bad: blobs are fetched lazily, so the first full-tree operation (e.g. grep across the whole repository) pays for fetching them. A session can prefetch with a plain `git fetch` after cloning, if needed.
- Bad: the runtime operates a git server and keeps mirrors fresh.

The shared-volume option was rejected because a pod's mounts are fixed at start, so grant changes wouldn't apply mid-session. Alternates also tie checkouts to the mirror's objects, so the mirror couldn't be garbage-collected safely.
