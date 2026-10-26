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

## More Information

Spike, 2026-10-26, claude-agent-acp 0.97.0 with Claude Code 2.1.310 and memini plugin 0.7.37:

- **Loading:** the plugin loads through the adapter's `_meta.claudeCode.options.plugins` (`{type: "local", path}`) on `session/new`. Its SessionStart hook ran and injected the memory directive, and its skills were listed.
- **Auth:** the MCP connection reached memini without a bearer. The plugin's `headersHelper` ran before the SessionStart hook had written its credentials file, and since Claude Code 2.1.238 plugin header helpers don't inherit credential env vars. memini answered 401, and Claude Code fell into OAuth discovery and gave up. That doesn't matter here, because the authorizer attaches the key, but the authorizer must add `Authorization` when it's missing, not only replace it.
- **Proxying:** the plugin's hooks use Node's `fetch`, which ignores `HTTPS_PROXY`. Transparent redirection at the pod boundary catches them anyway; an explicit-proxy enforcer would need `NODE_USE_ENV_PROXY=1`.
