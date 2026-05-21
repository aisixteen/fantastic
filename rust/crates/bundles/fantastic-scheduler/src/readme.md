# scheduler — recurring tasks as an agent
Persists schedules to `schedules.json` + appends fires to `history.jsonl` via the configured `file_agent_id`. Verbs: `schedule`, `unschedule`, `list`, `pause`, `resume`, `tick_now`, `history`. On every fire, emits `schedule_fired` to its own inbox AND the target's inbox.
