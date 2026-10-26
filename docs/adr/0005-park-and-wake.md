# 5. Sessions can be parked and woken

- Status: Accepted
- Date: 2026-10-26

## Context and Problem Statement

Some work has to wait for something outside the session, such as next week's version bumps or a PR landing. The session should be out of sight until then and come back by itself at the right moment, with its context intact.

## Decision Outcome

A session moves through the states `active → parked → woken → active` (or `done`).

- **Parking** pushes any work in progress to the session branch, keeps the transcript, frees the pod, and hides the session from the active board. The agent can propose parking itself, e.g. "park until the Renovate PRs for X land".
- **Wake conditions** can be:
  - a time
  - an event (a webhook, e.g. a PR opened or a release published)
  - activity on a linked object
  - a cheap check script run by the runtime on a schedule, without an LLM

  Conditions combine as "whichever comes first", plus a fallback timer, because webhooks miss events: GitHub sends no event for merge conflicts, for example.
- **Wake brief and triage:**
  - When parking, the agent writes a **wake brief**: what it is waiting for and what to check.
  - A wake first runs a small **triage** agent on the brief and the new event. Triage either re-parks the session with an updated brief or wakes the full session.
  - Waking a long transcript after the prompt cache has expired is expensive, so the full session wakes only when there's something to do.
- **Limits:** the number of wakes per session is capped, with backoff, to prevent loops.
- **Two wake primitives:** *wake the same session*, or *start a new session per trigger*, i.e. scheduled jobs.

### Consequences

- Good: deferred work doesn't clutter the active view, and comes back with its context.
- Good: re-parking after a look is cheap.
- Bad: triage adds a second agent path, and briefs can lose context the full transcript had.

## More Information

Prior art:

- Linear's snooze: "whichever comes first" between the time and new activity.
- Devin auto-resumes suspended sessions.
- Paseo has both schedules (a new agent per run) and heartbeats (the same agent).
- Claude Code's in-session cron has no catch-up for missed runs and expires after 7 days.
