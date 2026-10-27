# 21. Credentials live in the runtime

- Status: Accepted
- Date: 2026-10-27

## Context and Problem Statement

The authorizer attaches credentials on a session's behalf ([ADR 11](0011-enforcement-at-the-pod-boundary.md)): a Claude account, a GitHub App, memory keys. There will be several of each: switchable inference accounts ([ADR 20](0020-switchable-inference-accounts.md)), more than one GitHub identity, more than one user ([ADR 23](0023-users-and-ownership.md)). Config already lives in the runtime's database, not in git ([ADR 6](0006-self-reference.md)).

## Considered Options

- Deployment-level secrets (Helm values pointing at Kubernetes Secrets), one per credential
- Kubernetes Secrets the runtime creates and manages, referenced from the database
- Rows in the database, encrypted with a master key

## Decision Outcome

Chosen option: **rows in the database, encrypted with a master key**.

- A credential has an owner, a kind (`claude`, `github-app`, `memini`), a name, non-secret config (an app ID, a URL) and a secret. The first of a kind is the owner's default; grants can name another later.
- Secrets are sealed with XChaCha20-Poly1305 under one master key. The master key and the database URI are the only deployment-level secrets.
- Credentials are added, made default and deleted through the API and UI. The API never returns a secret. There's no import from the environment.

### Consequences

- Good: accounts are runtime config like everything else; adding one is a form, not a redeploy.
- Good: database backups only ever hold ciphertext.
- Bad: losing the master key makes every stored credential unreadable; rotating it needs a re-encryption pass, which doesn't exist yet.
- Bad: the runtime can read every credential, which it needs to anyway to attach them.
