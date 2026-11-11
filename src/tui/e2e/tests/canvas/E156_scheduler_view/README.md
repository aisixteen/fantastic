# E156 · scheduler_view — the canvas scheduler view over a real `scheduler.tools`

Scripted, no model, with a browser (`costs: free`). #507: the fantastic binary's own `scheduler.tools` (no stand-in), create, schedule, tick, unschedule.

A `ram_store.tools` target `e156_tgt` and a `scheduler.tools` agent `e156_sched` (granted to reach the target's `reflect`) are made and the scheduler is placed (its default view is the scheduler view).

| check | what |
|---|---|
| 0 | both agents are made; the view shows `nothing scheduled` |
| 1 | `schedule` (hourly) answers a `schedule_id`; after `⟳ refresh` the view lists it: id, target, verb, `every 3600s`, `runs 0`, `last: no runs yet` |
| 2 | `tick_now {schedule_id}` answers a fire with no error |
| 3 | the view, with no click (the `schedule_fired` event), shows `runs 1` and `last: ok` |
| 4 | `unschedule`, `⟳ refresh`: the view shows `nothing scheduled` again |
| 5 | no page error |
