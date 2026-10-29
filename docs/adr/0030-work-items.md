# 30. Work is tracked in sloppod as a graph of items

- Status: Proposed
- Date: 2026-10-29

## Context and Problem Statement

The order of work on sloppod lived in a GitHub issue, "Priorities" ([#255](https://github.com/bo0tzz/sloppod/issues/255)), kept there rather than in a session's context, which compaction erodes. A month of using it showed its limits:

- Every change was a proposal replacing the whole body; an item couldn't be ticked or moved on its own.
- It had no link to what was running: which sub-session was on what lived in the parent's context and in issue text.
- It drifted from reality between rewrites: items finished and pull requests merged, and the list caught up only when someone rewrote it.
- A flat list hides why things wait. "After #345 lands" or "needs a decision first" were prose, so nothing could tell what was ready to start.

The issues themselves drifted too: tracking issues listed work long since done, and nothing tied an issue to the pull request or session working on it.

Agents do most of the planning and bookkeeping, and a parent session's context isn't a place to keep it. Where should the work, its order and its state live, and who may change them?

## Decision Outcome

**Work is a graph of items kept by the runtime.** An item is something to get done; an edge says one item waits on another. What's ready is what nothing unfinished blocks.

- **Items:** a title, a short description, a state (`open`, `active`, `done`, `dropped`), a rank among its siblings, and links. An item can stand for a piece of design, a bug, or a group of other items (a parent, done when all its children are).
- **Edges:** `blocks` (B can't start until A is done) and `part of` (A is a child of B). Both are acyclic; adding an edge that would close a cycle is refused. Order among items that are ready at once is their rank, so the graph says what *can* go next and the rank what *should*.
- **A layer over issues, not a replacement.** An item can link a GitHub issue, which stays the record of what's wanted: its body, its discussion, outside contributors. The item holds what the issue can't: where it sits in the graph, its state, and what's working on it. An item needn't have an issue (a design step, a check), and an issue needn't have an item (the long tail).
- **Not tied to a session.** Items belong to their owner ([ADR 23](0023-users-and-ownership.md)), and later to a project ([ADR 1](0001-sessions-decoupled-from-directories.md)); sessions come and go while an item stays. A session that works on an item is linked to it, as are the pull requests and proposals that carry it out.
- **State follows what happens.** The runtime already watches pull requests ([ADR 4](0004-asynchronous-interaction.md), #107), sub-sessions' reports ([ADR 27](0027-sub-sessions.md)) and proposals ([ADR 3](0003-proposals-and-trusted-executor.md)). An item linked to them moves with them: `active` while a linked session works on it or a linked pull request is open, `done` when a linked pull request merges or a linked issue closes as completed. Nobody has to rewrite a list for it to be true.
- **Agents do most of the operating, under a grant.** Reading and changing the work graph is a capability ([ADR 2](0002-capabilities.md)), `work`, with a level:
  - `read`: see the graph.
  - `track`: also add items and edges, link sessions, pull requests and issues, and move the state of items. Enough for a session doing its share and filing what it finds.
  - `plan`: also rank, drop and reparent items. That's setting priorities.

  The owner always has `plan`. A sub-session gets at most its parent's level (attenuation), by default `track`. Like any grant, the owner sets and changes it per session.
- **Every change is recorded and can be undone.** Changes are events (who, which session, what, when), shown in the item's history, and a change can be reverted from there. That's what lets agents operate the graph without asking first: the owner sees and corrects after the fact, rather than approving each edit. A `plan` change by an agent is shown to the owner as a notice, so a reprioritisation doesn't go unseen.
- **Agents read it where they need it.** Tools on sloppod's MCP server (`list_work`, `update_work`, ...) at the session's level. A session linked to items is told them in its prompt, and the frontier (what's ready, by rank) is one call away, so a parent re-reads the plan after compaction instead of relying on memory.
- **People see it in the UI** as the graph and as the ready list, next to the sessions and proposals working on each item; the home dashboard ([#137](https://github.com/bo0tzz/sloppod/issues/137)) shows the frontier.

### Consequences

- Good: the plan is current without anyone rewriting it, says why things wait, and links to the sessions and changes carrying it out.
- Good: agents keep the plan in the runtime rather than in their context, so compaction and new parent sessions lose nothing.
- Good: issues go back to being records of what's wanted; tracking issues that list other issues aren't needed.
- Bad: two places to look, the issue for discussion and the item for state; the UI has to make the link obvious both ways.
- Bad: agents can reorder work without asking. The `plan` level, the notices and undo bound that, but a bad reprioritisation can stand until the owner looks.
- Bad: one more model to build, store and show, with its own API, tools and UI.

## More Information

Considered and not chosen:

- **GitHub issues only,** with labels and a tracking issue: what #255 was. No dependencies, whole-body edits, no link to sessions.
- **GitHub Projects:** fields, iterations and a board, but no dependencies between items, nothing sloppod can tie to its sessions and proposals, and every change a GitHub write, which sessions make only through proposals.
- **A flat ranked list:** simpler, but it loses why an item waits, which is the part that kept going stale in prose.
- **Every agent change as a proposal** (as #255 was): safe, but most changes are bookkeeping, and approving each one is the friction this replaces. Undo and notices are the check instead.

Open questions:

- Whether `blocks` should also be inferred, e.g. from stacked pull requests or a migration depending on another.
- Whether an item that links an issue mirrors its title and closes it when done, or only reads from it.
- How items move to projects once projects exist, and whether a sub-session's items are visible to its siblings.
- Whether `plan` changes by agents need the owner's approval after all, at least at first; the levels allow tightening without a redesign.
