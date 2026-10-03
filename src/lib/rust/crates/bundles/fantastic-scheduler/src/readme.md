# scheduler — recurring tasks
Verbs: schedule, unschedule, list, pause, resume, tick_now, history. Persistence (schedules.yaml, schedule_events.yaml) is automatic THROUGH the loader (`kernel_state`) — nothing to wire; a write failfasts only when no store is wired at the root.

**A schedule fires AS THE SCHEDULER.** Every fire goes out under the scheduler's
own id, so the gate judges the scheduler's `can_call` and the target's
`callable_by` for the scheduler: wire the scheduler → target edge. `list`,
`history`, `tick_now`, `unschedule` and `pause`/`resume {schedule_id}` see every
schedule.
