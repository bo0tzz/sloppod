# 25. Sessions move through their lifecycle by themselves

- Status: Proposed
- Date: 2026-10-28

## Context and Problem Statement

A session holds a pod, a workdir volume, keys and an egress policy while it lives, and those cost resources. Today the only transition sloppod makes by itself is purging a deleted session a week after it was deleted ([#144](https://github.com/bo0tzz/sloppod/pull/144)). A session that's been idle for days keeps its pod until the user parks it, and a parked one keeps its volume forever. The only way to free the volume is to delete the session, which throws the transcript away too. Users who keep every transcript (most of them, judging by the one we have) end up either paying for volumes they don't need or losing history they want.

What states should a session have, which transitions should happen without the user, and how does the user keep a session from being tidied away?

## Decision Outcome

A session is in one of five states. The first four hold a progressively smaller footprint; each automatic step only frees what can be rebuilt.

| State | Pod | Volume | Keys, egress policy | Transcript, grants, proposals | Listed |
|---|---|---|---|---|---|
| **active** (`starting`/`idle`/`working`/`waiting`) | yes | yes | yes | yes | sessions |
| **parked** | no | yes | yes | yes | sessions, marked parked |
| **archived** | no | no | no | yes | Archived section |
| **deleted** | no | no | no | yes, until purged | Settings → Deleted, restorable for 7 days |
| **purged** | — | — | — | gone | — |

- **Automatic transitions:**
  - An active session that's been **idle** (no turn, no open question or permission request, nothing queued) for **4 hours** is parked. Idle time counts from the session's last activity (`active_at`).
  - A session that's been **parked for 14 days** is archived.
  - A deleted session is purged after 7 days, as now.
  - The thresholds are runtime settings (the chart's `session.parkAfter`, `session.archiveAfter`), so a cluster can tune or turn them off.
- **Pinning:** a pinned session is never archived automatically; it can still be parked when idle, since parking keeps everything and wakes on a message. Pinning is a flag the user sets from the sidebar's context menu or the session menu, and pinned sessions sort to the top of the list.
- **Archiving** removes the pod, the volume, the session's keys (memini key, runtime secrets) and its egress policy, like a purge does, but keeps the session row, transcript, events, grants and proposals. An archived session is read-only in the UI: its transcript and proposals show, the composer doesn't.
- **Resuming** an archived session (from its menu, or by sending it a message, like waking) recreates it: a new pod and volume, its repositories cloned again at their session branches (which still exist on GitHub, holding anything that was pushed), its keys minted again, and the agent's own transcript restored from the runtime's copy (ADR 1), so `session/load` picks the conversation up. Uncommitted, unpushed work in the old volume is gone, which is why archiving waits two weeks after parking, and why the agent should push what matters before it goes idle.
- **Deleting** stays the explicit, heavy action, now from the context menu with a confirmation; an archived session can be deleted too.
- **The sidebar context menu** on a session offers, as they apply: Rename, Pin/Unpin, Park/Wake, Archive/Resume, Delete.
- **Wake conditions** from ADR 5 (a time, an event, a check script) are unchanged and still to come; they apply to parked sessions. An archived session isn't woken by a condition, only resumed by the user or by a message.

### Consequences

- Good: resources follow use. Idle sessions free their pod within hours, and old ones their volume within weeks, without the user doing anything.
- Good: no transcript is lost by tidying; only delete loses one, and only after a week.
- Good: pinning gives the user one switch for "keep this ready".
- Bad: resuming an archived session costs a clone and a cold start, and loses uncommitted work in the old volume.
- Bad: a session parked automatically mid-thought (the agent waiting on something it didn't say) needs its wake conditions, which aren't built yet; until then the user wakes it.

## More Information

Supersedes the "done" end state of ADR 5's `active → parked → woken → active (or done)`: "done" becomes archived, reached by time or by the user. ADR 5's wake conditions, brief and triage still apply to parked sessions.

Open questions:

- Whether a pinned session should also be exempt from auto-parking. Parking keeps everything, so this proposes not.
- Whether the thresholds belong per user (Settings) rather than per cluster.
- Whether archiving should first ask the agent to push work in progress (a notice: "this session will be archived; push anything worth keeping"), at the cost of a turn.
