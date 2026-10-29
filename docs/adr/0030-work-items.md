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

- **Items:** a title, a short description, a state (`open`, `active`, `done`, `dropped`), a rank among its siblings, and links. An item can stand for a piece of design, a bug, or a group of other items (a parent).
- **Edges:** `blocks` (B can't start until A is done) and `part of` (A is a child of B). Both are acyclic; adding an edge that would close a cycle is refused. Order among items that are ready at once is their rank, so the graph says what *can* go next and the rank what *should*.
- **Several graphs.** An owner keeps as many graphs as they have separate lines of work, e.g. one per project or effort. Sessions on unrelated projects work in different graphs and don't see each other's items. An item and its edges belong to one graph, and a `work` grant names the graph it covers.
- **A layer over issues, not a replacement.** An item should usually link a GitHub issue, which stays the record of what's wanted: its body, its discussion, outside contributors. The item holds what the issue can't: where it sits in the graph, its state, and what's working on it. An item needn't have an issue (a design step, a check), and an issue needn't have an item (the long tail).
- **Not tied to a session.** Graphs belong to their owner ([ADR 23](0023-users-and-ownership.md)), and later to a project ([ADR 1](0001-sessions-decoupled-from-directories.md)); sessions come and go while an item stays. A session that works on an item is linked to it, as are the pull requests and proposals that carry it out.
- **State follows what happens.** The runtime already watches pull requests ([ADR 4](0004-asynchronous-interaction.md), #107), sub-sessions' reports ([ADR 27](0027-sub-sessions.md)) and proposals ([ADR 3](0003-proposals-and-trusted-executor.md)), so items move with what's linked to them, and nobody has to rewrite a list for it to be true. A link either *completes* the item (the pull request or issue that finishes it) or is only *related* (earlier attempts, background, a session that looked into it). Then:
  - An item is `active` while a linked session works on it or a completing pull request is open.
  - An item is `done` once every completing link is finished (a pull request merged, an issue closed as completed) and every child is `done` or `dropped`. A completing pull request closed without merging stops counting as one, rather than finishing the item. A parent with no links of its own is done when its children are.
  - An item with no completing links is only done when someone says so, and saying so is refused while a child is unfinished.
  - Only `done` and `dropped` release what an item blocks, so a dependant never starts while part of the work it waits on is still open.
- **Agents do most of the operating, under a grant.** Reading and changing the work graph is a capability ([ADR 2](0002-capabilities.md)), `work`, for one graph, with a level:
  - `read`: see the graph.
  - `track`: also add items and edges, link sessions, pull requests and issues, and move items between `open`, `active` and `done` (within the rules above). Enough for a session doing its share and filing what it finds.
  - `plan`: also rank, reparent and drop items (any move into `dropped`, by whichever tool), and reopen dropped ones. That's setting priorities, so an agent's `plan` changes are proposals ([ADR 3](0003-proposals-and-trusted-executor.md)): the owner approves them, shown as a change to the graph, unless a standing approval for that session approves them automatically.

  The owner always has `plan`. A sub-session gets at most its parent's level (attenuation), by default `track`. Like any grant, the owner sets and changes it per session.
- **Every change is recorded and can be undone.** Changes are events (who, which session, what, when), shown in the item's history, and a change can be reverted from there. `track` changes apply at once: they're bookkeeping, and the owner sees and corrects them after the fact rather than approving each one. `plan` changes wait for approval as above, and one approved automatically is still shown to the owner as a notice, so a reprioritisation doesn't go unseen.
- **Agents read it where they need it.** Tools on sloppod's MCP server (`list_work`, `update_work`, ...) at the session's level. A session linked to items is told them in its prompt, and the frontier (what's ready, by rank) is one call away, so a parent re-reads the plan after compaction instead of relying on memory.
- **People see it in the UI** as the graph and as the ready list, next to the sessions and proposals working on each item; the home dashboard ([#137](https://github.com/bo0tzz/sloppod/issues/137)) shows the frontier.

### Consequences

- Good: the plan is current without anyone rewriting it, says why things wait, and links to the sessions and changes carrying it out.
- Good: agents keep the plan in the runtime rather than in their context, so compaction and new parent sessions lose nothing.
- Good: issues go back to being records of what's wanted; tracking issues that list other issues aren't needed.
- Bad: two places to look, the issue for discussion and the item for state; the UI has to make the link obvious both ways.
- Bad: reprioritising by an agent waits for the owner unless they turn on auto-approval for the session, and with it on, a bad reprioritisation can stand until the owner looks (the notices and undo bound that).
- Bad: one more model to build, store and show, with its own API, tools and UI.

## More Information

Considered and not chosen:

- **GitHub issues only,** with labels and a tracking issue: what #255 was. No dependencies, whole-body edits, no link to sessions.
- **GitHub Projects:** fields, iterations and a board, but no dependencies between items, nothing sloppod can tie to its sessions and proposals, and every change a GitHub write, which sessions make only through proposals.
- **A flat ranked list:** simpler, but it loses why an item waits, which is the part that kept going stale in prose.
- **Every agent change as a proposal** (as #255 was): safe, but most changes are bookkeeping, and approving each one is the friction this replaces. Only `plan` changes are proposals; `track` changes rely on undo instead.

Open questions:

- Whether `blocks` should also be inferred, e.g. from stacked pull requests or a migration depending on another.
- Whether an item that links an issue mirrors its title and closes it when done, or only reads from it.
- How items move to projects once projects exist, and whether a sub-session's items are visible to its siblings.
- Whether an item can block one in another graph, e.g. a shared dependency of two projects, or whether that's a sign they're one graph.

2026-10-29: stage 1 is built ([#384](https://github.com/bo0tzz/sloppod/issues/384)): graphs, items, edges, links and events in the database, the rules in one module, and the owner's REST API (`docs/api.md`, "Work"). Choices this ADR left open:

- **`part of` is a parent column.** An item is part of at most one other, so the parts form a tree. A part waits on what blocks the items it's part of, and an item waits on its parts. No edge may leave an item waiting on itself that way, so neither a parent nor a part can block the other.
- **Ranks are numbers.** A placed item gets a rank between its neighbours'. When there's no room left, the siblings are spread out again. Across the graph, the frontier orders items by their ancestors' ranks first.
- **Links carry a status:** `open`, `finished` or `abandoned`. Later stages set it by watching what the link points at; for now the owner sets it. A session link that's open makes its item active, as does an open completing pull request or proposal. Items remember whether the rules or a person set their state: what the rules made done or active goes back to open once nothing makes it so, while an item done by hand with nothing linked stays done. A state the rules would set anyway counts as theirs, so one an undo restores still follows its links.
- **Undo is an inverse event.** Undoing a change makes the inverse change and records it as an event that names the one it undoes. Undoing that event redoes the change.
  - A change can be undone only while its effect is still there.
  - Items are never deleted. Undoing an item's creation drops it.
  - State changes the rules make are recorded as automatic and can't be undone: they follow links and parts.
- **What's ready** is the open items with no unfinished parts that nothing unfinished blocks, directly or through an item they're part of, and that aren't inside a done or dropped item. Active items are under way, so they aren't in it.

2026-10-29: stage 2 is built ([#385](https://github.com/bo0tzz/sloppod/issues/385)): the `work` grant, sloppod's MCP tools, `plan` changes as a proposal kind, and linked items in the prompt. Choices:

- **A grant per graph.** `work` grants carry `{graph, name, level}`. The owner grants them from the session's grants list, `track` by default. A session can hold one per graph.
- **Tools by level.** `list_work` from `read`, and `update_work` from `track`. `propose_work_plan` is shown only at `plan`, and refused below it. A session without a grant sees none of them. Every call is checked against the graph its grant names, so an item in another graph can't be reached.
- **What `track` covers.** It adds and edits items (placed last among their siblings, since ranking is `plan`), moves them between `open`, `active` and `done`, adds and removes blocks, and links work. The rules of stage 1 still decide what a move may do. Moves into `dropped` and out of it are refused there, at any level, and point to the proposal. That is checked under the graph's lock, so a drop that lands between the agent's read and its change is never undone.
- **The plan proposal** (`sloppod.work.plan`) targets the graph's id, so a standing approval for the session and graph approves it. It's carried out change by change as the session's changes, so each is in the item's history and can be undone there. It's carried out only if the session still holds a `plan` grant of the graph then: one revoked or narrowed while it waited changes nothing.
  - An automatically approved one shows up for the owner as any proposal does: listed with `decided_by: standing-approval:<id>`, and broadcast. A separate notice for it waits for the UI work.
- **Attenuation.** A sub-session's grant of its parent's graph is at most `track`, and never more than the parent's.
- **The prompt** names the graphs a session is granted, with their levels, and the items it's linked to by a session link. It also says to read the frontier after a compaction. It's built when the agent starts, so a link made later is in the tool's answer, and in the prompt from the next start.
