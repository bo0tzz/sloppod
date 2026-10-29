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
  - it's never in a starting set, and never inherited by a sub-session ([ADR 27](0027-sub-sessions.md));
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
