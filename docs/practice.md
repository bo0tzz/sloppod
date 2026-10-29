# How we work with sloppod

What bo0tzz and the sessions working on sloppod have learned about working together, from dogfooding it. This is practice, not design: the ADRs say how sloppod works, this says how we use it. It changes through pull requests like anything else, and the durable parts are meant to end up in the agent's own instructions ([#35](https://github.com/bo0tzz/sloppod/issues/35)).

## Deciding what to work on

- **The work order lives outside a session's context,** which compaction erodes. It's moving into sloppod as a graph of work items ([ADR 30](adr/0030-work-items.md)); until that's built, the last agreed order is the closing comment on the old Priorities issue ([#255](https://github.com/bo0tzz/sloppod/issues/255)), and the parent session keeps it in memory. Read it before picking up work and after every compaction, and say so when switching away from it.
- **Standing rule:** work that speeds up development, in and on sloppod, comes first.
- **A new idea becomes a labelled issue, not work.** Start on something at once only when it's called urgent ("first priority", "fix this now").
- **Labels:** every issue gets an area label (`ui`, `harness`, `proposals`, `security`, `credentials`, `memory`, `ci`, `adr`, `prompt`, `testing`, `enforcement`), plus `dogfood` when it came from using sloppod, and `priority` or `needs discussion` where they apply. Check `gh label list`; don't invent labels.
- **Design first where it's needed:** a change that contradicts or extends an ADR gets the ADR (or an amendment) proposed and discussed before it's built.
- **Look before adding:** search the issues before filing one, and the ADRs before proposing one. A new ADR that restates an existing one is a dated amendment to that one instead, with only what's new.
- **Decide what you can:** within agreed work, make the design calls yourself, say what you decided in the PR, and move on. Ask only what only the user can answer.

## Talking to the user

- **Decisions needed from the user go through `ask_user`**, not prose in a reply, where they scroll away and can't be told apart from a status update. Requests to act, like "merge this by hand", stay plain messages with the link.
- **Check before claiming:** look up a pull request's state (`gh pr view <n> --json state,mergedAt`) right before saying it's waiting on the user. Things get merged fast.
- **Make GitHub actions links,** e.g. the pull request to merge.
- **Don't treat your own recommendation as the user's decision.** If they haven't answered, it's still open.
- **Keep updates short:** what changed, what's waiting on them, and nothing they already know.

## Running sub-sessions

The parent session manages, and sub-sessions ([ADR 27](adr/0027-sub-sessions.md)) do the work. The parent writes briefs, integrates, keeps the backlog and talks to the user. It codes only what can't be briefed faster than done, and even small changes go to an idle sub-session.

- **One sub-session per issue.** A finished one can be reused for a follow-up in the same area: its volume keeps the checkout and build caches warm. Spawn a fresh one if its context looks muddled.
- **Add sub-sessions slowly,** and talk the layout through with the user before a new batch.
- **Keep them busy:** hand a finished sub-session its next task right away. "Idle" only means no turn is running; a sub-session waiting on its own background job (a Playwright run) shows idle too, so ask before assuming it's stuck.
- **Before spawning,** bring the parent's session branch up to date with main. Sub-sessions start from it, and reports compare against it.
- **A brief says:**
  - read `CLAUDE.md` and the ADRs that apply;
  - start from the latest `origin/main`;
  - run the checks CI runs;
  - commit with pull-request-ready messages;
  - push to `<parent branch>-<own id>-<name>`;
  - when done, send the branch, its commits and a pull-request description.

  Use `report_done` with `head` for anything but the session branch.
- **Branches:** a sub-session's branches are named after the parent's, so the parent proposes them as pull requests directly; no copying commits. Sub-sessions rebase onto main and resolve conflicts themselves, since they know their change best. Force pushes aren't allowed, so a rebased branch gets a new name (`-2`).
- **Review findings go back to the sub-session that wrote the change.** It builds on the pull request's branch and pushes its own branch; the parent fast-forwards the PR branch and asks for the next round.
- **Sub-sessions can't message each other;** the parent passes on what one needs from another, word for word where it's a design.
- **The parent allocates migration numbers** across sub-sessions working at the same time, so no two pending migrations share one. A migration that depends on another (alters its table, say) gets a higher number, since a fresh database runs them in numeric order, and the two changes are proposed as a stack so the prerequisite merges first. Independent migrations don't need renumbering above ones already shipped: sqlx applies any migration not yet applied, whatever its number.

## Proposals and merging

- Pull requests and stacks are proposed with `review: true`. Issue proposals and updates can't be reviewed.
- A proposal's branches can't change. If a branch has to move (a rebase that would need a force push), withdraw and propose again.
- Pushing to a pull request after its review turns auto-merge off unless the change is the same; it then needs merging by hand.
- **Integration pitfalls** we've hit:
  - Squash-merged bases leave their children conflicting, though main has the same content: merge main in and keep the child's side where `git diff <base> origin/main -- <file>` is empty.
  - A conflict-free merge can still be wrong: read the result, e.g. an early return from one change that defeats another.
  - `git checkout --theirs <file>` takes the other side's whole file and silently drops this branch's other changes in it.
  - When a pull request merges, GitHub deletes its head branch. A sub-session building on it has to move to `origin/main`, so tell it as soon as it merges.
  - Never build a commit from a sub-session's whole tree; its base may be older than main, which would revert what merged since. Cherry-pick or merge, then check `git diff origin/main --stat` lists only the change's files.

## Environment

- Put work in `/workspace` (e.g. git worktrees under `/workspace/wt/`), never `/tmp`, which a pod restart wipes.
- "The user doesn't want to take this action right now. STOP…" as a tool result is Claude Code's cancel text, not a rejection: something cancelled the running tool call. On 2026-10-28 that was usually a notice steered into the call ([#259](https://github.com/bo0tzz/sloppod/issues/259), fixed by #264, which makes notices wait). But a user's own steer still interrupts a tool call on purpose, and Esc or Stop cancel the turn. So before calling it the bug, check what came in at that moment: a steered message from the user, or a cancel in the runtime's log (cancels and permission answers are logged with who sent them). If it was the user, treat it as their instruction.
- Sessions can push workflow files once the push token asks for `workflows: write` (in progress).
- **After a cluster roll** (node drains, upgrades): look for proposals stuck at `approved`, whose outcome couldn't be recorded (#326), and for pull requests whose head push guard turned auto-merge off. An MCP server that failed to connect when the agent started stays gone until its next start; memini can still be reached directly at `$MEMINI_BASE_URL/mcp` meanwhile.
- **When a safety classifier stops a response,** its content mustn't be produced again, even reworded, by this session or by handing it to another. Stop, tell the user what was being done when it was stopped, and let them decide what happens to that work.
