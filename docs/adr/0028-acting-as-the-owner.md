# 28. Sessions act as their owner only through grants the owner controls

- Status: Proposed
- Date: 2026-10-28

## Context and Problem Statement

Everything a session does on GitHub today, it does as sloppod's GitHub App ([ADR 21](0021-credentials-live-in-the-runtime.md)): git over the session's grants, `gh` reads, and the pull requests and issues its proposals open. That has two limits:

- **Reach:** the App only reaches repositories it's installed on. The owner's own repositories in an organization where they can't install it, or on an account they don't want it on, are out of reach.
- **Identity:** some of GitHub only answers to a user, not an App installation: a user's own Projects boards, gists, and other user-owned resources. Work on those has to be done as the owner.

It isn't for putting an agent where people expect a person: comments, reviews or contributions to other people's projects that would pass an agent's work off as the owner's own.

The answer to both is the owner's own GitHub credential, a personal access token ([#54](https://github.com/bo0tzz/sloppod/issues/54)). But a session that can act as its owner is a different trust level from one that acts as a bot: what it does is attributed to a person, and it reaches whatever that person can. [ADR 2](0002-capabilities.md) already describes grants with policies, durations and revocation, but sessions today get a fixed set at start, plus repositories through `request_repository`, and the owner can't see or change them while the session runs.

## Decision Outcome

**Acting as the owner is a grant, never ambient, and the owner controls it explicitly.**

- **A user credential.** The owner adds a fine-grained GitHub personal access token in Settings, as a credential of kind `github-user` ([ADR 21](0021-credentials-live-in-the-runtime.md), [ADR 23](0023-users-and-ownership.md)): stored by the runtime, never in the pod. Classic tokens are refused, since they can't be narrowed to repositories.
- **A grant to act as the owner** (`as-owner`), per repository, with an access level: `read`, `push` (git push to the session's branches), `propose` (pull requests, issues and comments opened as the owner, through proposals as now). It's a capability like any other ([ADR 2](0002-capabilities.md)), with these rules:
  - Its grant policy is always `ask`: never in a session's starting set, never granted by `auto`, and a standing approval, which belongs to one session (see the note in [ADR 2](0002-capabilities.md)), only covers it if it names `as-owner` explicitly.
  - It's never inherited: a sub-session starts without it whatever its parent holds ([ADR 27](0027-sub-sessions.md)), and has to ask.
  - It can have an expiry, and the owner can revoke it at any time. Revocation takes effect at the authorizer at once ([ADR 11](0011-enforcement-at-the-pod-boundary.md)).
- **Grants decide reach, not the token.** Which repositories a session may act on as the owner is set by its `as-owner` grants, chosen per session; what the PAT can reach is only the upper bound, checked when a grant is made. A PAT that reaches fifty repositories gives a session none of them until it's granted some.
- **One identity per repository.** For each repository, a session's requests go out as the owner where it holds an `as-owner` grant, and as the App otherwise. The authorizer attaches the token per request, as it does the App's, and only for the repositories granted.
- **Proposals say who acts.** A proposal on a repository where the session acts as the owner is executed with the owner's token and says so wherever it's shown ("opens as bo0tzz"), so approving it is approving an action in the owner's name. Reviews ([ADR 24](0024-automated-review-before-human-review.md)) are unchanged.
- **The owner sees and controls a session's grants.** The session view lists every grant: what it allows, as whom, since when and until when ([#112](https://github.com/bo0tzz/sloppod/issues/112)). From there the owner can grant, narrow, extend and revoke ([#70](https://github.com/bo0tzz/sloppod/issues/70)). An agent requests `as-owner` the way it requests a repository today, with a reason, and the request shows what acting as the owner means.
- **Audit.** Every request made with the owner's token is logged with the session and grant it was made under.

### Consequences

- Good: sloppod can work on repositories the App can't reach, and act as the owner where that's what's wanted, with the owner deciding each time.
- Good: the grant controls this needs are ones every capability wants anyway; `as-owner` is the first that can't work without them.
- Bad: fine-grained PATs expire and are managed by hand on GitHub; the owner has to keep one current. A GitHub App's user-to-server OAuth tokens don't replace them for reach: they only cover repositories the App is installed on. They could replace them for identity on repositories it is.
- Bad: two identities per session make it possible to be surprised by which one acted; showing the identity on every grant and proposal is the mitigation.

## More Information

Considered and not chosen:

- **Using the PAT only where the App isn't installed:** simpler, but it conflates reach with identity; acting as the owner on a repository the App can reach would be impossible, and acting as the owner would happen silently wherever the App is missing.
- **Ambient PAT for the whole session:** the default for most tools (`gh auth login` in a dev container), and the opposite of [ADR 2](0002-capabilities.md).
- **Classic PATs:** they can't be narrowed to repositories, so every session holding one could reach everything the owner can.
- **An OAuth App ("Connect GitHub"), for now:** it reaches everything the owner can, without a token to paste or renew, but its scopes can't be narrowed to repositories, so the stored token could do anything the owner can if it leaked, and organizations may block it. Grants still decide what sessions reach, so it fits the same model; it can be added later as a second kind of `github-user` credential, after fine-grained PATs.

To decide during implementation: whether `propose` as the owner also covers merging, and whether a session may hold `as-owner` on a repository while its proposals there still open as the App.

2026-10-28: the grant controls this relies on are built (ADR 2's dated note): grants have an expiry, and the owner grants, narrows, extends and revokes them while the session runs. The `as-owner` grant itself is still to come.

2026-10-28: [ADR 29](0029-grants-name-their-credential.md) generalises this: every grant names the credential it acts with, so acting as the owner is a `repo` grant whose credential is a `github-user` token, not an `as-owner` capability. The rules above apply to every grant held with a person's credential. The two questions left open here are answered there: an identity does all of what it's granted, merging included, and splitting operations between identities is the direction, through separate grants for now.
