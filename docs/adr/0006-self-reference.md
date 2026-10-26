# 6. The runtime is self-referential

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Agents are well placed to notice that a capability is missing, an instruction is wrong, or a profile should exist. They should be able to see how the runtime is configured and suggest changes to it, without being able to change it unilaterally.

## Decision Outcome

The runtime's API is exposed to sessions as a capability.

- **Read (ambient):** a session can see its own grants, the capabilities it could request, its profile, and the parts of the runtime config that apply to it.
- **Propose (gated):** changes to runtime config are a proposal kind ([ADR 3](0003-proposals-and-trusted-executor.md)). This covers:
  - new or changed capability definitions
  - profiles
  - projects: creating them, and re-filing sessions
  - instruction snippets
  - standing approvals
  - proposal kinds

  The human reviews a diff of the config.
- **Applied changes affect new sessions only.** Running sessions are never reconfigured underneath themselves, except for grant changes, which are explicit and delivered through the inbox.
- **Restart picks up new config.** The runtime re-places the session in a fresh pod with config rendered from the current state, and resumes the same transcript ([ADR 1](0001-sessions-decoupled-from-directories.md)). The human can trigger this, or the agent can propose it, typically after its own config proposal is applied. Restarts are a clean break: background processes stop, the working tree survives on the session's volume and branch, and the next turn starts with a cold prompt cache.
- **Storage:** config lives in the database ([ADR 15](0015-postgres.md)). Revisions can be kept as rows if we want history; branching isn't needed.

### Consequences

- Good: the system can improve its own setup through the same review flow as everything else.
- Bad: config proposals need a good diff renderer, and the instructions agents receive are themselves editable by agents. Those edits must stay behind review.

## More Information

Prior art: Claude Projects shows "setup recommendations" for the user to confirm. Devin queues knowledge suggestions for review. Conductor, Sculptor and Codex environments apply config changes only to new tasks.
