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
| **deleted** | no | kept until purged, if it had one | kept until purged | yes, until purged | Settings → Deleted, restorable for 7 days |
| **purged** | — | — | — | gone | — |

- **Automatic transitions:**
  - An active session that's been **idle** (no turn, no open question or permission request, nothing queued) for **4 hours** is parked. Idle time counts from the session's last activity (`active_at`).
  - A session that's been **parked for 14 days** is archived.
  - A deleted session is purged after 7 days, as now.
  - The thresholds are runtime settings (the chart's `session.parkAfter`, `session.archiveAfter`), so a cluster can tune or turn them off.
- **Waiting sessions aren't archived:** a parked session with a pending wake condition (ADR 5: a time, an event, a check) is exempt from automatic archiving until the condition fires or is cleared, however long that takes. The 14 days count from then.
- **Pinning:** a pinned session is never archived automatically; it can still be parked when idle, since parking keeps everything and wakes on a message. Pinning is a flag the user sets from the sidebar's context menu or the session menu, and pinned sessions sort to the top of the list.
- **Archiving** removes the pod, the volume, the session's keys (memini key, runtime secrets) and its egress policy, like a purge does, but keeps the session row, transcript, events, grants and proposals. An archived session is read-only in the UI: its transcript and proposals show, the composer doesn't.
- **Resuming** an archived session (from its menu, or by sending it a message, like waking) recreates it: a new pod and volume, its repositories cloned again at their session branches (which still exist on GitHub, holding anything that was pushed), its keys minted again, and the agent's own transcript restored from the runtime's copy (ADR 1), so `session/load` picks the conversation up. Uncommitted, unpushed work in the old volume is gone, which is why archiving waits two weeks after parking, and why the agent should push what matters before it goes idle.
- **Deleting** stays the explicit, heavy action, now from the context menu with a confirmation, and keeps today's contract: the session is parked and marked deleted, keeps whatever it had (its volume and keys too) until it's purged a week later, and restoring it brings it back parked with its work intact ([ADR 22](0022-memini-keys-per-session.md)). An archived session can be deleted too; it has no volume or keys left to keep.
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

2026-11-03: idle parking isn't built yet for work sessions. Sub-sessions park by themselves sooner, 5 minutes after they've finished (their report carried out, nothing of theirs in review) and gone quiet, as ADR 27's note of this date says ([#284](https://github.com/bo0tzz/sloppod/issues/284)), since they hold their tree's limited slots.

2026-11-06: sessions make room for each other ([#454](https://github.com/bo0tzz/sloppod/issues/454)). Every session pod asks for the same resources at the same priority, so Kubernetes never preempts one for another, and a pod it can't place waited until something parked of its own accord: a parent waited on a sub-session that couldn't be scheduled while idle sessions held the room. The runtime now looks every 30 seconds at the session pods the cluster can't place, through the placement backend (`PlacementBackend::unschedulable`, from the pod's `PodScheduled` condition for agent-sandbox). A session whose pod has been unschedulable for 20 seconds says why (`unschedulable`). When it's for lack of resources (`Insufficient memory`, `cpu`) and has been for 90 seconds, the runtime parks quiet sessions to make room, one for each such pod at a time, less the parked sessions whose pods are still going (their room is on its way): idle for 5 minutes, with nothing open for the user and no turn, queue or command of the agent's, kept reviewers first, then the longest quiet. Any other reason (a taint or affinity, a volume that can't attach, a quota) parks nothing. While any session pod can't be placed for lack of resources, no pod is replaced for a new image.

2026-11-06: sessions leave nothing behind they don't need ([#455](https://github.com/bo0tzz/sloppod/issues/455)). A parked session's egress policy is removed once its pod is gone (it was kept, as a deny-all fence, for as long as the session stayed parked); waking applies it again. Its Sandbox stays: it owns the workspace volume, which agent-sandbox deletes with it. A stopped session, which can't start again, has any pod ended (fenced in while it goes) and then loses its policy, listener and keys (`released_at` records it; a parked one is released under its transition lock, so a wake can't lose the policy it applies), and its volume a week after it stopped unless the user keeps it (`volume_kept`). An hourly sweep removes objects whose session is purged or unknown, through `PlacementBackend::sessions` and `Enforcer::sessions` (ADR 7). Each session's supervisor reports its workspace volume's use (`Disk` on the link, every 5 minutes), which the runtime keeps with the session, so a parked one still shows it. docs/deploy.md says what each phase keeps.
