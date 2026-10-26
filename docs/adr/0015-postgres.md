# 15. Postgres is the source of truth

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Somewhere needs to hold sessions, event logs, transcript blobs, capability definitions, grants, proposals, approvals, standing approvals and runtime config.

## Considered Options

- Postgres
- CRDs in etcd, reconciled by a controller
- SQLite on a PVC

## Decision Outcome

Chosen option: **Postgres**, run with CloudNativePG. It could share a cluster with memini ([ADR 8](0008-shared-memory-service.md)).

Kubernetes objects are *derived*: Sandboxes and pods, CiliumNetworkPolicies, Secrets, bound tokens and namespaces. They carry labels with the session and grant IDs, and owner references where possible so they are garbage-collected, and the runtime reconciles them against the database.

### Consequences

- Good: one transactional store for everything, including large transcripts and event logs. Config edits and history are just rows.
- Bad: state isn't visible through `kubectl`. Drift between the database and the cluster has to be reconciled by the runtime.
