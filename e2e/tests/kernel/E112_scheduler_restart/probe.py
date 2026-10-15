"""kernel/scheduler_restart — the scheduler fires, bounds a fire, and keeps its
schedules across a restart. NO MODEL.

The PACKAGED host of `RUNTIME` (python | rust | swift), every call over `web_rest`;
the scheduler `sch` fires AS ITSELF at the `slow` probe agent `tgt` (the edge
granted on both ends):

  1. A FIRE: a 1 s schedule of `work` fires on its own — `history` records the
     target's reply, `list` counts the runs;
  2. A BOUNDED FIRE: `fire_timeout: 1` on a schedule of `sleep 30` — `tick_now`
     returns within the bound with a `timed_out` error and no result; the same
     schedule firing on its own records `timed_out` in `history` while the 1 s
     schedule keeps firing beside it;
  3. A SCHEDULE OF ANOTHER AGENT'S VERB IT IS NOT GRANTED is refused and recorded;
  4. RESTART (SIGTERM, boot the same directory): every schedule comes back with
     its target, payload, interval, fire_timeout and paused flag, run counts never
     go backwards, the history from before is still there; a schedule created just
     before the stop that never fired fires after the boot; the 1 s schedule fires
     again; a paused schedule does not.
"""

from __future__ import annotations

import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
REACH = ["sch", "tgt"]
#: The fields of a schedule that must survive a restart unchanged.
KEPT = ("id", "target", "payload", "interval_seconds", "paused", "fire_timeout")


def poll(fn, bound: float = 15.0, step: float = 0.1):
    end = time.monotonic() + bound
    v = fn()
    while not v and time.monotonic() < end:
        time.sleep(step)
        v = fn()
    return v


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-sched-"))
    bad: list[str] = []
    kernels: list[HostKernel] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def boot() -> HostKernel:
        hk = HostKernel(spec, work, reach=REACH).boot()
        kernels.append(hk)
        return hk

    def schedules(hk: HostKernel) -> dict[str, dict]:
        return {s["id"]: s for s in hk.send("sch", {"type": "list"}).get("schedules") or []}

    def history(hk: HostKernel, sid: str) -> list[dict]:
        r = hk.send("sch", {"type": "history", "schedule_id": sid, "limit": 500})
        return r.get("history") or []

    def schedule(hk: HostKernel, tag: str, **args) -> str | None:
        r = hk.send("sch", {"type": "schedule", **args})
        sid = r.get("schedule_id")
        need(isinstance(sid, str), f"schedule {tag}: {str(r)[:200]}")
        return sid

    try:
        hk = boot()
        r = hk.create(parent_id="rest", handler_module="slow_probe.tools", id="tgt",
                      callable_by={"$self": "*", "$parent": "*", "sch": ["work", "sleep"]})
        need(r.get("id") == "tgt", f"create tgt: {str(r)[:200]}")
        r = hk.create(parent_id="rest", handler_module="scheduler.tools", id="sch",
                      can_call={"$self": "*", "tgt": "*"},
                      callable_by={"$self": "*", "$parent": "*"})
        need(r.get("id") == "sch", f"create sch: {str(r)[:200]}")

        # 1 · a fire on its own
        every = schedule(hk, "every 1 s: work", target="tgt", payload={"type": "work"},
                         interval_seconds=1)
        fired = poll(lambda: [e for e in history(hk, every) if e.get("error") is None])
        first = (fired or [{}])[0]
        need(bool(fired) and isinstance((first.get("result") or {}).get("runs"), int)
             and first.get("target") == "tgt" and first.get("payload") == {"type": "work"},
             f"it fires on its own; history records the reply: {str(first)[:200]}")
        n = (schedules(hk).get(every) or {}).get("run_count")
        need(isinstance(n, int) and n >= 1, f"list counts the runs: run_count={n}")

        # 2 · a bounded fire
        hung = schedule(hk, "sleep 30, fire_timeout 1, hourly", target="tgt",
                        payload={"type": "sleep", "seconds": 30}, interval_seconds=3600,
                        fire_timeout=1)
        t0 = time.monotonic()
        t = hk.send("sch", {"type": "tick_now", "schedule_id": hung}, timeout=60)
        took = time.monotonic() - t0
        need(t.get("fired") is True and t.get("result") is None
             and str(t.get("error") or "").startswith("timed_out"),
             f"tick_now of a hung target: a timed_out error, no result: {str(t)[:200]}")
        need(took < 10, f"tick_now returns within the bound ({took:.1f} s for a 30 s sleep)")
        h = history(hk, hung)
        need(len(h) == 1 and str(h[0].get("error") or "").startswith("timed_out"),
             f"history records the bounded fire: {str(h)[:200]}")
        auto = schedule(hk, "sleep 30, fire_timeout 1, every 1 s", target="tgt",
                        payload={"type": "sleep", "seconds": 30}, interval_seconds=1,
                        fire_timeout=1)
        before = len(history(hk, every))
        timed = poll(lambda: [e for e in history(hk, auto)
                              if str(e.get("error") or "").startswith("timed_out")])
        need(bool(timed), f"the hung schedule firing on its own records timed_out: {str(timed)[:200]}")
        more = poll(lambda: len(history(hk, every)) >= before + 2)
        need(bool(more), f"the 1 s schedule keeps firing beside it ({before} → {len(history(hk, every))})")
        r = hk.send("sch", {"type": "unschedule", "schedule_id": auto})
        need(r.get("removed") is True, f"unschedule the hung one: {r}")

        # 3 · a verb the scheduler is not granted on its target
        denied = schedule(hk, "a verb tgt does not accept from sch", target="tgt",
                          payload={"type": "started"}, interval_seconds=3600)
        d = hk.send("sch", {"type": "tick_now", "schedule_id": denied})
        need(d.get("result") is None and str(d.get("error") or "").startswith("failed")
             and "does not accept 'started' from sch" in str(d.get("error")),
             f"the gate refuses the fire, and tick_now says so: {str(d)[:200]}")

        # 4 · restart
        paused = schedule(hk, "paused, every 1 s", target="tgt", payload={"type": "work"},
                          interval_seconds=1)
        r = hk.send("sch", {"type": "pause", "schedule_id": paused})
        need(r.get("paused") is True, f"pause one schedule: {r}")
        late = schedule(hk, "every 3 s, created just before the stop", target="tgt",
                        payload={"type": "work", "tag": "late"}, interval_seconds=3)
        old = schedules(hk)
        need((old.get(late) or {}).get("run_count") == 0, "the late one has not fired before the stop")
        hk.close()
        booted = time.time()
        hk = boot()
        new = schedules(hk)
        need(set(new) == set(old), f"every schedule comes back: {sorted(new)} vs {sorted(old)}")
        for sid, s in old.items():
            got = new.get(sid) or {}
            diff = {k: (s.get(k), got.get(k)) for k in KEPT if s.get(k) != got.get(k)}
            need(not diff, f"{sid}: target/payload/interval/paused/fire_timeout kept: {diff or 'equal'}")
            need((got.get("run_count") or 0) >= (s.get("run_count") or 0),
                 f"{sid}: run_count never goes backwards ({s.get('run_count')} → {got.get('run_count')})")
        need(len(history(hk, hung)) == 1, "the history from before the stop is still there")
        lf = poll(lambda: [e for e in history(hk, late) if e.get("ts", 0) >= booted - 1
                           and e.get("error") is None])
        need(bool(lf), f"the late schedule fires after the boot: {str(lf)[:200]}")
        ef = poll(lambda: [e for e in history(hk, every) if e.get("ts", 0) >= booted])
        need(bool(ef), f"the 1 s schedule fires again after the boot ({len(ef or [])} fires)")
        pr = (schedules(hk).get(paused) or {})
        need(pr.get("paused") is True and pr.get("run_count") == (old.get(paused) or {}).get("run_count")
             and not history(hk, paused),
             f"the paused schedule stays paused and never fired: {str(pr)[:200]}")
        rf = hk.send("sch", {"type": "reflect"})
        need(rf.get("running") is True, f"the fire loop runs after the boot: {rf}")
    finally:
        for k in kernels:
            k.close()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
