# scheduler — recurring tasks
Verbs: schedule, unschedule, list, pause, resume, tick_now, history. Persistence (schedules.yaml, schedule_events.yaml) is automatic THROUGH the loader (`kernel_state`) — nothing to wire; a write failfasts only when no store is wired at the root.

**A schedule fires AS ITS CREATOR** — never as the scheduler. `schedule` records the
caller as the schedule's `owner` (a local agent id; a foreign `<guid>|<agent>` with
`via`, the bridge leg that relayed it; `null` for the operator) and every fire runs
under that identity, so the gate judges the creator's `can_call` and the target's
`callable_by` for the creator: a schedule reaches only what its creator could. Wire
the creator → target edge (and the target's `callable_by` for the creator), not the
scheduler's. `list`, `history`, `tick_now`, `unschedule` and `pause`/`resume
{schedule_id}` act on the caller's OWN schedules (the operator: all) — another's
schedule is not found. `schedule_fired` events carry `owner`.

**Cutting a peer off includes unscheduling its schedules.** A foreign caller's
schedule fires as its `<guid>|<agent>` and never passes the leg that relayed it, so
sealing that leg or re-pinning its `peer_guid` does not stop it — `unschedule` it too
(the operator's `list` shows every owner's schedules).

**A call carrying secret marks is not scheduled** — `schedule` refuses it
(`reason: unauthorized`): a schedule keeps its payload and every fire re-sends it
unmarked. Send a secret-bearing call yourself, marked, each time.
