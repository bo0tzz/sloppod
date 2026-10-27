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

- **The runtime decides namespaces, not the session.** memini scopes every request by the `X-Memini-Namespace` header (its primary, write namespace) and reads through a fixed cascade: ancestors, then the home namespace, then links. The runtime chooses each session's primary namespace from its projects ([ADR 1](0001-sessions-decoupled-from-directories.md)):
  - **One project:** that project's namespace. A project for a repository defaults to `owner/repo`, which is what memini derives for that repository outside sloppod, so memory carries over.
  - **Several related projects** (e.g. the `immich-app/*` repositories): their common parent namespace. `scope: everywhere` reaches the children.
  - **Unrelated projects:** the project the session was filed under first. The others are reachable only through memini links.
  - **No project:** a holding namespace. When the session is filed, the runtime moves the memories that session wrote to the project.
- **Home is identity.** memini binds the home namespace to the API key and ignores `X-Memini-Home` for a bound key. The runtime uses a key bound to the person's home (`personal/<user>`). With several users, it would use an unbound key and set `X-Memini-Home` per session owner.
- **The authorizer enforces it** ([ADR 11](0011-enforcement-at-the-pod-boundary.md)). It attaches the key, overwrites `X-Memini-Namespace` with the runtime's choice, allows the endpoints the agent's memory tools use, and refuses memini's administration endpoints: pins, settings, keys, links, and moving, splitting or deleting namespaces.
- **Claude Code uses memini's plugin.** The runtime sets `MEMINI_NAMESPACE` so the plugin's own handshake agrees. Left alone, the plugin derives the namespace from the checkout's git remote, which in a pod is the runtime's mirror ([ADR 19](0019-repositories-from-runtime-mirrors.md)). memini's behaviour settings (turn capture, digests, injection) live on the server per key, so the runtime's key carries them.
- It stores data in Postgres with VectorChord.
- It ships a Helm chart.
- It needs an OpenAI-compatible embeddings endpoint. An LLM is optional; it does background consolidation.

### Consequences

- Good: memory is shared across sessions and pods, and is independent of machines and paths.
- Bad: memini records everything automatically through hooks and consolidates later. Existing memory is a few hundred hand-curated notes, so expect noisier context at session start. Trial it on one repository before migrating.
- Note: memini API keys establish identity, not isolation: any key can read every namespace. Sessions therefore never hold memini keys, and isolation is enforced by the authorizer as above. The only memini behaviour this restricts is its escape hatches: explicit `namespaces` lists on REST reads are limited to the session's read set. Memory still never holds secrets; memini's own scrubber also redacts anything that looks like a credential, including commands such as `GH_TOKEN=$(…)`.
- Bad: young project: created June 2026, mostly one author, AGPL-3.0.
- Note: its import tool covers past transcripts. Existing curated memory files need a separate, one-off import.

## More Information

Spike, 2026-10-26, claude-agent-acp 0.97.0 with Claude Code 2.1.310 and memini plugin 0.7.37:

- **Loading:** the plugin loads through the adapter's `_meta.claudeCode.options.plugins` (`{type: "local", path}`) on `session/new`. Its SessionStart hook ran and injected the memory directive, and its skills were listed.
- **Auth:** the MCP connection reached memini without a bearer. The plugin's `headersHelper` ran before the SessionStart hook had written its credentials file, and since Claude Code 2.1.238 plugin header helpers don't inherit credential env vars. memini answered 401, and Claude Code fell into OAuth discovery and gave up. That doesn't matter here, because the authorizer attaches the key, but the authorizer must add `Authorization` when it's missing, not only replace it.
- **Proxying:** the plugin's hooks use Node's `fetch`, which ignores `HTTPS_PROXY`. Transparent redirection at the pod boundary catches them anyway; an explicit-proxy enforcer would need `NODE_USE_ENV_PROXY=1`.

MVP, 2026-10-27: sessions reach memini through the runtime instead of the egress listener. The plugin's `MEMINI_BASE_URL` is the runtime's sessions port (`/memini`), where the same request policy decides each request and the runtime forwards it to the owner's memini. memini can then live at an in-cluster address, which Cilium's FQDN rules don't match under kube-proxy replacement and the TLS-intercepting listener couldn't serve, and adding a memini doesn't change the listener. The policy is unchanged: the session's own key, the pinned namespace, administration refused.

memini server behaviour (v0.7.37 source), which the decisions above rely on:

- The namespace is `X-Memini-Namespace`, else the key's default namespace, else the server default.
- The home is the key's bound home if it has one (the header is then ignored, with an `X-Memini-Warning`), else `X-Memini-Home`.
- `POST /v1/handshake` resolves the namespace from what the client reports (pin, then `MEMINI_NAMESPACE`, then declared, then derived from the git remote, then key default) and ignores the header. It also returns the behaviour settings.
- The MCP `memory_recall` tool takes only `scope` (`project`, `full` or `everywhere`), no namespace list.
