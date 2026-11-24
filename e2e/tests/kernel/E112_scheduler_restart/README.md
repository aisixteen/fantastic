# `scheduler_restart` (E112) — fires, bounded fires, schedules across a restart

Free: no model. The packaged rust host, every call over `web_rest`; a
`scheduler` (`sch`) fires AS ITSELF at the `slow` probe agent `tgt`, by one line
on the caller (`sch`'s `can: {tgt: {call: [work, sleep]}}`); `tgt` holds no line
and keeps no list of callers:

1. a 1 s schedule of `work` fires on its own — `history` holds the reply,
   `list` counts the runs;
2. `fire_timeout: 1` on `sleep 30`: `tick_now` comes back within the bound with a
   `timed_out` error; the same schedule on a 1 s cadence records `timed_out` while
   the 1 s `work` schedule keeps firing beside it;
3. a schedule of a verb `sch`'s line does not cover (`started`): `tick_now`
   reports the gate's unknown answer for `tgt` (`no agent 'tgt'`), and history
   records it;
4. SIGTERM, boot the same directory: every schedule back with its target,
   payload, interval, fire_timeout and paused flag; run counts never go backwards;
   history from before the stop is kept; a schedule created just before the stop
   (never fired) fires after the boot; the 1 s one fires again; a paused one does
   not; the fire loop runs.

The parity `bundle_verbs` / `wire` families cover the verbs' replies and one fire;
this covers the bound and the restart.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id scheduler_restart --lanes 3
RUNTIME=rust uv run python ../../../e2e/tests/kernel/E112_scheduler_restart/probe.py
```
