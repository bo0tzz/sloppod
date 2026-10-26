# 8. Memory is a shared service

- Status: Accepted (pending a trial)
- Date: 2026-10-26

## Context and Problem Statement

Claude Code's built-in auto memory is a set of files per machine, keyed by project path. It doesn't follow a session into a pod, doesn't move with renamed projects, and isn't shared between machines.

## Considered Options

- Claude Code's built-in file memory, synced to the pods somehow
- [memini](https://github.com/eleboucher/memini): a self-hosted memory service, used through MCP and agent plugins

## Decision Outcome

Chosen option: **memini**. Claude Code's built-in auto memory is turned off (`autoMemoryEnabled: false` / `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`).

What memini offers:

- Namespaces follow the session's context ([ADR 1](0001-sessions-decoupled-from-directories.md)):
  - writes go to the namespace of the session's project
  - reads also cover the git-remote namespaces of repositories the session holds grants for (memini's own keying), and the user's personal namespace

  memini's linked namespaces and read scopes can express this directly.
- It stores data in Postgres with VectorChord.
- It ships a Helm chart.
- It needs an OpenAI-compatible embeddings endpoint. An LLM is optional; it does background consolidation.

### Consequences

- Good: memory is shared across sessions and pods, and is independent of machines and paths.
- Bad: memini records everything automatically through hooks and consolidates later. Existing memory is a few hundred hand-curated notes, so expect noisier context at session start. Trial it on one repository before migrating.
- Bad: memini API keys establish identity, not isolation: any key can read every namespace. Memory therefore cannot be scoped per session and must never hold secrets.
- Bad: young project: created June 2026, mostly one author, AGPL-3.0.
- Note: its import tool covers past transcripts. Existing curated memory files need a separate, one-off import.
