# scheduler — recurring tasks
Verbs: schedule, unschedule, list, pause, resume, tick_now, history. Persistence (schedules.json, history.jsonl) is automatic THROUGH the loader (`kernel_state`) — nothing to wire; a write failfasts only when no store is wired at the root.
