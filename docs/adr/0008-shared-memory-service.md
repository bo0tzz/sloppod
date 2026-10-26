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

- Namespaces follow the session's project ([ADR 1](0001-sessions-decoupled-from-directories.md)), using memini's own scoping model:
  - **Writes:** the session's primary namespace (`X-Memini-Namespace`) is its project's namespace, and writes land there.
  - **Personal memory:** your personal namespace is the "home" namespace (`X-Memini-Home`, bound to the key), which every read includes.
  - **Related repositories and shared conventions** are memini *links* from the project namespace, configured per project and read through memini's normal cascade (parent namespaces, then home, then links).

  memini expects the client to set these headers. In sloppod, the authorizer ([ADR 11](0011-enforcement-at-the-pod-boundary.md)) takes on that role, so memini's server-side cascade works unchanged.
- It stores data in Postgres with VectorChord.
- It ships a Helm chart.
- It needs an OpenAI-compatible embeddings endpoint. An LLM is optional; it does background consolidation.

### Consequences

- Good: memory is shared across sessions and pods, and is independent of machines and paths.
- Bad: memini records everything automatically through hooks and consolidates later. Existing memory is a few hundred hand-curated notes, so expect noisier context at session start. Trial it on one repository before migrating.
- Note: memini API keys establish identity, not isolation: any key can read every namespace. Sessions therefore never hold memini keys. Their memini traffic goes through the per-request authorizer ([ADR 11](0011-enforcement-at-the-pod-boundary.md)), which attaches the key and pins the namespaces a request may read or write to the session's project and grants, so isolation is enforced there. The only memini behavior this restricts is its escape hatches: explicit per-call `namespaces` lists and `scope:"everywhere"` subtree reads are limited to the session's read set. Memory still never holds secrets.
- Bad: young project: created June 2026, mostly one author, AGPL-3.0.
- Note: its import tool covers past transcripts. Existing curated memory files need a separate, one-off import.
