# 29. Every grant names the credential it acts with

- Status: Accepted
- Date: 2026-10-28

## Context and Problem Statement

A grant says what a session may reach ([ADR 2](0002-capabilities.md)). It doesn't say as whom. Every capability uses "the owner's one credential of that kind", the default ([ADR 21](0021-credentials-live-in-the-runtime.md)):

- `model` uses the default Claude account;
- `codex` uses the default ChatGPT login;
- `memory` uses the default memini;
- `github` and `repo` use the GitHub App.

That's fine with one credential of each kind, and it breaks as soon as there are more:

- **Several accounts of a kind** ([#276](https://github.com/bo0tzz/sloppod/issues/276)): two Claude accounts, or a work and a personal ChatGPT login. Today a session uses whichever is the default when it asks, and usage and limits are kept under the default even when another one was used.
- **Acting as a person** ([ADR 28](0028-acting-as-the-owner.md), [#54](https://github.com/bo0tzz/sloppod/issues/54)): a personal access token is a different identity from the App, with different reach. ADR 28 made it a capability of its own, `as-owner`. But the owner may add more than one token, for different accounts or narrowed to different repositories, and "the owner" doesn't say which.
- **Falling back** ([#252](https://github.com/bo0tzz/sloppod/issues/252)): when one account hits its limit, another could be used. That's a choice of credential, and today nothing records it.

These are one question: which credential a grant acts with.

## Decision Outcome

**A grant names the credential it acts with.** Credentials are the identities a session can act as. A grant binds a capability, its scope (a repository, a namespace) and, where it matters, the operations it covers, to one of the owner's credentials.

- **Grants carry a credential.** A grant's params gain `credential`, the id of one of the owner's credentials of a kind the capability takes:
  - `model` takes a Claude account;
  - `codex` takes a ChatGPT login;
  - `memory` takes a memini;
  - `github` and `repo` take a GitHub App or a GitHub user token (`github-user`).

  Capabilities without a credential, like `packages` and `docs`, don't change.
- **The default is filled in when the grant is made, not when it's used.** A starting set ([ADR 2](0002-capabilities.md)) names no credentials. Each grant gets the owner's default of its kind when it's made, and keeps that credential until the grant is changed. Making another credential the default changes what new grants get, not what running sessions use. That's what "default" means from now on, as [#276](https://github.com/bo0tzz/sloppod/issues/276) asks.
- **The owner sees and changes it.** The session's grants list shows each grant's credential by name ("model: Claude, work account"). The owner can switch a grant to another credential of the same kind, the way they narrow or extend it. Switching the model's credential mid-session is the account switch of [ADR 20](0020-switchable-inference-accounts.md).
- **Usage follows the credential.** Quota observations, usage limits and the waits for their reset are kept per credential, the one the session's grant names ([#263](https://github.com/bo0tzz/sloppod/issues/263), [#251](https://github.com/bo0tzz/sloppod/issues/251)).
- **Acting as a person is a credential, not a capability.** ADR 28's `as-owner` becomes a `repo` grant whose credential is a `github-user` token. The rules ADR 28 set for it apply to every grant whose credential is a person's identity rather than a service's:
  - its grant policy is always `ask`;
  - it's never in a starting set, and never inherited by a sub-session ([ADR 27](0027-sub-sessions.md)) (since 2026-10-29: inherited only to read and push the child's own branches, below);
  - a standing approval only covers it if it names that credential;
  - every request made with it is logged with the session and grant.

  What the token reaches is the upper bound, checked when the grant is made. The grants decide what a session actually reaches.
- **An identity does the whole of what it's granted.** A `repo` grant's credential is used for every operation the grant covers on that repository: reads, pushes, and the proposals executed there ([ADR 3](0003-proposals-and-trusted-executor.md)). That includes enabling auto-merge on a pull request it opened. The App isn't involved in what a person's token does, and the reverse holds too. Proposals say who they act as ("opens as bo0tzz").
- **Operations can be split later.** A `repo` grant covers a set of operations (`read`, `push`, `propose`). The direction is that each operation on a repository can have its own credential: push as the owner, open pull requests as the App. For now one grant names one credential. A session that should push as the owner while its proposals still open as the App holds two grants, with different operations.

### Consequences

- Good: several accounts of a kind work without special cases. Each session uses the one its grant names, and usage, limits and fallbacks follow it.
- Good: acting as a person needs no capability of its own. It's the same grant controls, with stricter rules for person-held credentials.
- Good: what a session acts as is visible and changeable per grant, instead of hiding behind a global default.
- Bad: every reader of a grant now resolves its credential, and a deleted credential leaves grants pointing at nothing. Those grants stop working, are shown as broken, and the owner switches them to another credential.
- Bad: grants made before this have no credential. They're migrated to the owner's current default of each kind.

## More Information

To decide before building:

- **A credential, or a role that maps to one?** A grant could name a role ("work GitHub", "personal Claude") that the owner points at a credential. Then replacing an expired token is one change, not one per grant. But it adds a level of indirection to every grant, and roles would need their own management. Leaning towards credentials directly, plus "replace this credential's secret" (a new token for the same credential) for renewals.
- **Fallback (#252):** a grant naming an ordered list of credentials, or a per-owner rule. An ordered list on the grant keeps it visible where the decision is made.
- **Does the App become a grantable identity too?** Today the App is the implicit credential of `github`. Under this ADR it's explicit, so a repository could be "this PAT only". That falls out of grants naming credentials. The open part is whether to show it that way in the UI.

The order of work: the GitHub half first (a `repo` grant with a `github-user` credential: [#54](https://github.com/bo0tzz/sloppod/issues/54)), since that's where a second identity is needed now. Then the model and codex credentials per grant, with [#276](https://github.com/bo0tzz/sloppod/issues/276).

2026-10-28: the GitHub half is built. A `repo` grant can name one of the owner's `github-user` tokens. The authorizer uses it for git and reads on that repository (#301), and proposals there are read and carried out with it, with the identity fixed when they're proposed. The agent asks with `request_owner_identity`, a proposal kind of its own that no standing approval covers (approving picks the identity, which is a person's call; stricter than the rule above until standing approvals can name a credential), and the user picks the token when approving. Still to come: commit authorship. Commits made in the pod still name the App's bot as their author.

This supersedes ADR 28's `as-owner` capability, but not its rules or reasoning, which are the rules for person-held credentials above.

2026-10-29: "never in a starting set" means never *suggested*. A new session's starting set can be given as grants (`POST /sessions` with `grants`), and a repository grant there can name one of the owner's tokens. That's the owner choosing it, as in the grants list. The judgement that suggests a starting set never picks a credential.

2026-10-29: GraphQL reads use the owner's token when they name only repositories granted with it, and the token is narrow (below) ([#340](https://github.com/bo0tzz/sloppod/issues/340)). Its path, `/graphql`, names no repository, so until then GraphQL always read as the App, and a session whose grants all name a token read it unauthenticated, which GitHub refuses. The authorizer now parses the query for its `repository(owner:, name:)` lookups, at any depth and in every operation and fragment, with owner and name taken from literals or the request's `variables` (`gh` passes them as variables). The token is attached when there is at least one, all are granted with the same token, and every root field is such a lookup. Anything else reads as the App, as before: a query that reads `viewer`, `search` or `node(id:)` from the root, which would reach whatever the token reaches; a lookup whose owner or name it can't tell; a repository granted as the App, or with another token, or not at all. REST is unchanged: a path naming a repository granted with a token reads with it.

Parsing bounds where a query starts, not where it goes. GraphQL's graph is connected: from a granted repository, a query can follow its owner, its forks or a pull request's author to other repositories the token reaches. So GraphQL also needs a narrow token, one that reaches nothing the session isn't granted with it. The token's recorded reach (the repositories listed when it was added) must be complete (`more_repositories` false), and every repository in it must be granted to the session with that same token. With any other token, GraphQL reads as the App, which can't read repositories only the token reaches. `gh`'s GraphQL commands then fail on those repositories, and the agent reads them over REST (`gh api repos/...`), where the path names the repository and the token is used. The grants decide reach, so a token that reaches more than them isn't used where the request can't be bounded. The recorded reach is as of when the token was added; widening a token on GitHub afterwards isn't seen until it's added again. Parsing deeper, following what a query can reach from each lookup, could relax this later.

2026-10-29: "never inherited by a sub-session" becomes "inherited only to read the repository and push the child's own branches" ([#393](https://github.com/bo0tzz/sloppod/issues/393)). A session acting on a repository with the user's token, on one where the GitHub App isn't installed, spawned a sub-session. The child got the grant without the token, so it acted as the App and couldn't push its branch or even report it. Now a child's grant of such a repository keeps the parent's token, narrowed to two of the operations this ADR anticipated: `read` (clone, fetch, REST reads that name the repository) and `push` (its own branches, `<parent branch>-<child>`, which the authorizer enforces ref by ref). The grant records that as `operations: ["read", "push"]`; a grant without `operations` covers everything, as before.

- Not `propose`. The child's outward proposals on that repository are refused when they're made, and again if one is carried out. Its report isn't outward, so it goes through, and the parent proposes the work. A reviewer of the report (or of any proposal on a repository its author reads with a token) reads the repository with the same token, limited to `read`.
- A parent grant that already lists its operations passes on only `read` and `push` of them, never more.
- Not GraphQL with the token. The narrow-token rule (above) only counts grants in full, so a child's GraphQL reads there use the App's token, and it reads over REST.
- This is attenuation ([ADR 2](0002-capabilities.md)): the child can push nothing its parent couldn't already, and its pushes land where the parent integrates them. The token's other rules still hold: it's never suggested in a starting set, and every request made with it is logged with the session and grant.

A repository grant whose identity can't reach its repository is pointed out when the grant is made: the App isn't installed there, or the token's recorded reach leaves it out. For now that's when a sub-session is spawned, in the tool's result. Granting through the API, a starting set or `request_repository` already refuses such a grant.

2026-10-29: the model half is built ([#276](https://github.com/bo0tzz/sloppod/issues/276)). A `model` or `codex` grant names an ordered list of the owner's accounts of its kind, `params.credentials`, rather than one: the shape fallback ([#252](https://github.com/bo0tzz/sloppod/issues/252)) needs, which this ADR leaned towards above. When the grant is made it lists all of the owner's accounts of the kind, the default first, and keeps the list until the owner changes it. That's done where every grant is stored, so starting sets, grants from the grants list, review sessions and the rest all get it, and a sub-session's copy keeps its parent's list. Existing grants were migrated the same way.

- The authorizer attaches the first account on the list that still exists. One deleted is skipped; a grant whose accounts are all gone is refused with a message saying so, until the owner gives it another. A grant made before the owner had an account of the kind names none and uses the default.
- Usage follows it: Claude Code's `usage_update`s are kept under the account the session's grant uses, every ChatGPT login is polled (not just the default), and a session waiting for a usage limit waits for its own account's reset.
- The grants list shows each model grant's accounts by name, the one in use first, and the owner reorders ("Use first"), removes or adds them. Putting another first is ADR 20's switch: the next request goes out on it, and a session waiting for the old account's limit follows the new one's.
- The runtime's judgements ([ADR 26](0026-runtime-inference.md)) always use the owner's default Claude account, whatever their utility session's grant says: they're the owner's, not a session's.
