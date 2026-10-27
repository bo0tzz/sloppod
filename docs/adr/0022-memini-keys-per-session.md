# 22. memini keys per session

- Status: Accepted
- Date: 2026-10-27

## Context and Problem Statement

memini keys can be bound to a home namespace, have a default namespace, be admin or read-only, and carry their own settings. They don't isolate: any key can read any namespace, so isolation stays with the authorizer ([ADR 8](0008-shared-memory-service.md)). Which key should session traffic use?

## Considered Options

- The admin key on every session request, with the authorizer restricting calls
- One ordinary key for all sessions
- The admin key for management only, and a key minted per session

## Decision Outcome

Chosen option: **the admin key for management, a key per session for traffic**.

- The memini credential ([ADR 21](0021-credentials-live-in-the-runtime.md)) holds the admin key, used only by the runtime: creating and deleting session keys now, and later links and moving memories when a session is filed into a project.
- Each session with a `memory` grant gets its own key, named `sloppod-<session>`, bound to the owner's home, defaulting to the session's namespace, and read-only for read-only grants. The authorizer attaches it and still pins the namespace header.
- Deleting a session deletes its key.

### Consequences

- Good: a gap in the authorizer can't reach key management; session keys aren't admin.
- Good: memini's activity names the session that acted, settings can differ per session, and revoking one session's memory is one key.
- Bad: one memini key per session to create and clean up. A runtime that dies between creating a session and storing its key leaves an orphan; nothing sweeps those yet.
