# 30. Capabilities are definitions, not code

- Status: Proposed
- Date: 2026-10-29

## Context and Problem Statement

[ADR 2](0002-capabilities.md) describes a capability as a declarative bundle of parts:
- tools;
- a credential provider;
- a request policy;
- network reach;
- MCP servers;
- an instruction snippet;
- a grant policy.

What's built is a fixed set of names, each wired by hand in several places:
- `repo` has params and branch rules of its own;
- `memory` mints a key when the pod is placed;
- `docs` hard-codes Context7's server and its egress;
- `packages` is egress plus a request policy;
- `model` and `codex` attach one credential each;
- each host's request policy is Rust.

Adding a capability means touching the policies, egress, the agent's MCP config, grant descriptions and the UI. The names and params are whatever each one grew into. Nothing can be added without changing sloppod.

The case that shows it: the owner runs VictoriaLogs and VictoriaMetrics, and wants the session developing sloppod to read production logs and metrics through their MCP servers. Later, sessions that operate on the cluster will want the same. A general development session has no use for them. Sloppod shouldn't know or care what those servers are: the owner should be able to say "this MCP server, reachable here, with this credential" and grant it to the sessions that need it.

## Decision Outcome

**A capability is a definition: data that names its parts.** The runtime assembles a session's reach from the definitions of its grants. It doesn't branch on which capability it is.

- **A definition holds:**
  - a name, unique per owner;
  - a description, which the agent's `list_grants` and the UI show;
  - and any of these parts:
    - **MCP servers:** a URL, and headers to attach from a credential ([ADR 29](0029-grants-name-their-credential.md)). The agent gets them in its MCP config, as `docs` does today. The authorizer attaches the credential outside the pod ([ADR 21](0021-credentials-live-in-the-runtime.md)), so the token never enters it.
    - **Egress:** hosts the session may reach, with a request policy: a built-in one by name (`read-only` for GET and HEAD only, `all`), or an extension's ([ADR 7](0007-extension-points.md)).
    - **Credential:** a credential kind the grant names one of (ADR 29), attached to the egress or MCP requests as the definition says (a header, a bearer token).
    - **Tools:** a mise fragment ([ADR 12](0012-tooling-via-mise.md)).
    - **Instructions:** a snippet appended to the agent's prompt while the grant holds.
    - **Params:** a schema for what a grant of it can narrow, e.g. a repository or a namespace, which the parts can refer to.
    - **Grant policy:** `ask` (the default), `auto`, `ambient` (in every starting set) or `never` ([ADR 2](0002-capabilities.md)).
- **The owner manages definitions** in Settings and through the API, as they do credentials. A definition is theirs alone ([ADR 23](0023-users-and-ownership.md)). Granting it uses the controls that already exist: expiry, narrowing, revocation, and the agent asking for it with a reason.
- **The built-in capabilities become definitions that ship with the runtime:** `model`, `codex`, `github`, `repo`, `packages`, `docs` and `memory`. Where one needs behaviour no definition can express, its definition names an extension that provides it, so the core still doesn't branch on names:
  - git push rules and proposals for `repo`;
  - minting a memini key for `memory`;
  - GitHub tokens scoped to the grants for `github`.

  Their names stay, so existing grants keep working.
- **A user-defined MCP server is the plainest definition:** an MCP part, the egress to reach it, and a credential if it needs one, with policy `ask`. VictoriaLogs and VictoriaMetrics are two such definitions. Sloppod passes them through without knowing what they serve.

### Consequences

- Good: the owner adds what their sessions may reach without a change to sloppod, and sessions get only what they're granted.
- Good: one model for every capability. Descriptions, grant controls, expiry and the starting set work the same for all of them.
- Good: the built-ins' special cases are named, as extensions, instead of being scattered.
- Bad: a definition can open egress the owner didn't think through. Mitigations: the request policies are named and few, `ask` is the default, and a definition shows what it opens before it's granted.
- Bad: migrating the built-ins touches the authorizer, placement and the agent's config at once. It's done in steps, with one definition at a time behind the existing names.

## More Information

To decide while building:
- **Per owner or per deployment:** whether the operator can ship definitions for every user, e.g. a cluster's own observability, as well as owners defining their own.
- **MCP servers through the authorizer or through the supervisor:** the authorizer attaching headers works for HTTP servers reachable from the pod. Servers inside the cluster may be easier for the runtime to proxy, as it does memini.
- **How params reach the parts:** e.g. a LogsQL filter a grant narrows to. The simplest version has no params for user definitions, and the narrowing comes later.

The order of work:
1. Definitions as data, with user-defined MCP servers (the VictoriaLogs and VictoriaMetrics case).
2. `docs` and `packages` moved onto definitions, being the simplest built-ins.
3. The rest, one at a time.
