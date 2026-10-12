# scheduler — recurring tasks
Verbs: schedule, unschedule, list, pause, resume, tick_now, history. Persistence (schedules.yaml, schedule_events.yaml) is automatic THROUGH the loader (`kernel_state`) — nothing to wire; a write failfasts only when no store is wired at the root.

**A schedule fires AS THE SCHEDULER.** Every fire goes out under the scheduler's
own id, so the gate judges the scheduler's `can_call` and the target's
`callable_by` for the scheduler: wire the scheduler → target edge. `list`,
`history`, `tick_now`, `unschedule` and `pause`/`resume {schedule_id}` see every
schedule.

**Cadence.** Each schedule has its own `interval_seconds`; the loop wakes at the earliest due
fire and never on a fixed tick, so schedules at 5 s and at 10 min coexist in one scheduler. To
change a cadence, `unschedule` and `schedule` again. A schedule can fire any verb — e.g. an
`exec_bridge` `start` of a one-shot check job, for a source that does not push (a
job's own output has `monitor` on its `exec_bridge`).
