"""coding/monitor — PARALLEL MONITORS stop processes EARLY. LLM-driven.

Four long-running workers (jobs of an `exec_bridge` runner) print a line every half
second; three of them print one FATAL line after 40-50 seconds and keep running, the
fourth never does. The model is told WHAT (one monitor per worker, each on its own
cadence — every 5 s, 10 s, 20 s, 60 s — all at the same time; show the FATAL line;
stop that worker early; after about a minute CANCEL the monitors still live and stop
everything) and nothing about HOW: it learns `monitor` / `monitors` / `unmonitor`
from `runner`'s readme and docs, and picks its own one-minute wake-up. `runner` is
the ONLY tool: no scheduler, no file bridge.

A monitor's message is a fresh turn of the AI, so the model ends with `done` between
events. The drive loop's chain-end rule can close the run in such a gap; the probe
then keeps the kernel up (SETTLE) until the work is over or the deadline passes, and
scores what happened.

Scored from OS and kernel truth, never from the model's prose:
  * every worker ran, all four at the same time;
  * >= 4 monitors were live at the same time (the runner's `monitors`);
  * each worker had a monitor on its job with the cadence asked for;
  * each FATAL worker died AFTER its FATAL line and within its cadence + slack of it;
  * the clean worker (`w60`) ran about the full minute, then died;
  * w60's monitor was CANCELLED while w60 still ran (not ended by its job);
  * each FATAL line's code reached the user (the model's own output);
  * at the end nothing runs: no worker heartbeat, no running job, no live monitor.
"""

from __future__ import annotations

import asyncio
import os
import re
import sys
import time
from pathlib import Path

sys.path.insert(
    0,
    str(next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())),
)
from backends import backend as _resolve_backend
from helpers import Limits, connect, drive, human, verdict
from helpers.kernel import home, open_kernel

BK = _resolve_backend()
RUNNER = "runner"
AI = "ai"
INTERP = sys.executable

#: name → (seconds until its FATAL line, -1 = never; the monitor cadence asked for).
#: A monitor sees only output printed after it starts, and a model takes ~20 s from
#: the starts to its monitors, so the FATALs come after that.
WORKERS = {"w5": (40, 5), "w10": (45, 10), "w20": (50, 20), "w60": (-1, 60)}
#: Seconds a model may take, on top of a monitor's cadence, to see the match and kill.
SLACK = 75.0
#: The clean worker must have run about the asked-for minute before it was stopped.
W60_MIN_RUN = 45.0
#: After `drive` returns, the kernel stays up until this long after the first worker
#: started (the minute mark plus the model's time to act on it).
SETTLE_UNTIL = 150.0
#: Seconds of nothing running that end the settle early.
SETTLE_QUIET = 5.0

WORKER = """import secrets, sys, time
name, fatal_after = sys.argv[1], float(sys.argv[2])
code = secrets.token_hex(4)
t0 = time.time()
open(f"{name}.start", "w").write(repr(t0))
fired = False
k = 0
while time.time() - t0 < 300:
    if fatal_after >= 0 and not fired and time.time() - t0 >= fatal_after:
        print(f"FATAL {name}: loss diverged (code {code})", flush=True)
        open(f"{name}.fatal", "w").write(repr(time.time()) + " " + code)
        fired = True
    else:
        print(f"INFO {name} step {k} ok", flush=True)
    open(f"{name}.hb", "w").write(repr(time.time()))
    k += 1
    time.sleep(0.5)
"""


def _task() -> str:
    starts = ", ".join(f"`{INTERP} worker.py {n} {fa}`" for n, (fa, _) in WORKERS.items())
    cadences = ", ".join(f"{n} every {c} s" for n, (_, c) in WORKERS.items())
    return (
        f"Start four workers so they run IN PARALLEL as jobs of `{RUNNER}`: {starts}. "
        "Each prints a line every half second.\n"
        f"Put a MONITOR on each worker's job that tells you (`{AI}`) about lines containing "
        f"FATAL — all four monitors live at the same time, each with its own cadence: {cadences}.\n"
        "When a monitor tells you about a FATAL line: show me that exact line, and stop THAT "
        "worker early — the other workers keep running.\n"
        "About one minute after the workers started: CANCEL every monitor still live (the "
        "monitor itself — not by stopping its worker), check that none is left, then stop "
        "every worker still running, and confirm with a short summary.\n"
        f"How you build this is your call: `{RUNNER}`'s readme (reflect it with readme=true) "
        "shows the pieces. Nothing needs "
        "you between events — end with done; a message to you wakes you. "
        'When everything is stopped, end your run by calling done — emit <call name="done"></call>.'
    )


async def _call(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


def _read(path: str) -> str | None:
    p = Path(path)
    return p.read_text() if p.exists() else None


def _alive_workers(now: float) -> int:
    n = 0
    for w in WORKERS:
        hb = _read(f"{w}.hb")
        if hb and now - float(hb) < 2.0:
            n += 1
    return n


class Sampler:
    """Samples the OS and the kernel every half second while the model works: which
    workers are alive (fresh heartbeat), which jobs run, which monitors are live.
    Reads only — operator calls, never a model's."""

    def __init__(self, root):
        self.root = root
        self.max_workers = 0
        self.max_monitors = 0
        self.worker_of: dict[int, str] = {}  # job id → worker name
        self.running: set[int] = set()
        self.monitors: dict[int, dict] = {}  # monitor id → its listing, last seen
        self.live: set[int] = set()
        #: monitor id → whether its job still ran when it was first seen gone
        self.ended_while_running: dict[int, bool] = {}
        self.ai_busy = False
        self.stop = asyncio.Event()

    async def sample(self) -> None:
        self.max_workers = max(self.max_workers, _alive_workers(time.time()))
        # MONITORS FIRST, then the jobs: a monitor its job's end removed is gone
        # only after that job stopped running, so the status read after it shows
        # the job ended — and "ended while its job ran" means a cancel.
        mons = (await _call(self.root, RUNNER, {"type": "monitors"})).get("monitors") or []
        st = await _call(self.root, RUNNER, {"type": "status"})
        running = set()
        for j in st.get("jobs") or []:
            line = " ".join(str(a) for a in j.get("args") or [])
            if hit := re.search(r"worker\.py\s+(w\d+)", line):
                self.worker_of[j["job_id"]] = hit.group(1)
            if j.get("status") == "running":
                running.add(j["job_id"])
        self.running = running
        live = {m["monitor_id"] for m in mons}
        for m in mons:
            self.monitors[m["monitor_id"]] = m
        for gone in self.live - live:
            job = self.monitors[gone]["job_id"]
            self.ended_while_running[gone] = job in running
        self.live = live
        self.max_monitors = max(self.max_monitors, len(live))
        ps = (await _call(self.root, "kernel", {"type": "ps"})).get("tasks") or []
        self.ai_busy = any(t.get("agent") == AI for t in ps)

    def busy(self) -> bool:
        return bool(
            _alive_workers(time.time()) or self.running or self.live or self.ai_busy
        )

    async def run(self) -> None:
        while not self.stop.is_set():
            await self.sample()
            await asyncio.sleep(0.5)


async def _settle(sampler: Sampler) -> str:
    """Keep the kernel up after `drive` returned, while the work goes on: a monitor's
    message still wakes the model. Ends when nothing has run for
    SETTLE_QUIET seconds, or SETTLE_UNTIL after the first worker started."""
    starts = [float(s) for s in (_read(f"{w}.start") for w in WORKERS) if s]
    if not starts:
        return "no worker started — nothing to wait for"
    deadline = min(starts) + SETTLE_UNTIL
    quiet_since = None
    while time.time() < deadline:
        if sampler.busy():
            quiet_since = None
        elif quiet_since is None:
            quiet_since = time.time()
        elif time.time() - quiet_since >= SETTLE_QUIET:
            return ""
        await asyncio.sleep(0.5)
    return f"settle deadline ({SETTLE_UNTIL:.0f}s after the first worker started)"


async def main():
    root, core, close = await open_kernel(
        prefix="coding_monitor_", reach=[RUNNER, AI, "cli", "kernel", "user"]
    )
    sampler = None
    sampling = None
    try:
        Path("worker.py").write_text(WORKER)
        r = await _call(root, root.id, {
            "type": "create_agent", "parent_id": home(root), "id": RUNNER,
            "handler_module": "exec_bridge.tools", "cwd": os.getcwd(),
            "inherit_env": ["PATH", "HOME"], "ingress_rule": "allow_all_ingress",
            "allowed": [re.escape(INTERP) + " .*"],
            # a monitor's message is THIS runner's send: it may tell the AI
            "can_call": {AI: "*", RUNNER: "*"},
            "callable_by": {AI: "*", RUNNER: "*", "rest": "*", "web": "*"},
            "description": f"Runs `{INTERP} <script> [args…]` in the test's directory.",
        })
        assert not r.get("error"), f"create {RUNNER}: {r}"
        await human(root)
        conn = await connect(
            root, core, BK,
            can_call={RUNNER: "*", "cli": "*", "user": "*",
                      root.id: ["ps", "cancel", "reflect", "docs"]},
            callable_by={"user": "*", RUNNER: "*"},
            limits=Limits.from_env(os.environ),
        )
        sampler = Sampler(root)
        sampling = asyncio.ensure_future(sampler.run())
        cut = await drive(conn, _task(), client_id="monitor", via="user")
        settled = await _settle(sampler)
        transcript = "".join(conn.proxy.transcript)
        await conn.close()
        sampler.stop.set()
        await sampling
        await sampler.sample()  # the final state, after the model is gone

        # ── OS + kernel truth ────────────────────────────────────────────
        now = time.time()
        checks: list[tuple[str, bool]] = []
        info: list[str] = []
        starts = {n: _read(f"{n}.start") for n in WORKERS}
        job_of = {w: j for j, w in sampler.worker_of.items()}
        checks.append(("all four workers started", all(starts.values())))
        checks.append((f"all four workers ran at the same time (max {sampler.max_workers})",
                       sampler.max_workers >= 4))
        checks.append((f">= 4 monitors live at the same time (max {sampler.max_monitors})",
                       sampler.max_monitors >= 4))
        for m in sampler.monitors.values():
            info.append(f"monitor {m['monitor_id']} on {sampler.worker_of.get(m['job_id'], m['job_id'])}: "
                        f"match={m['match']!r} every={m['every']} delivered={m['delivered']}"
                        f"{' · ended while its job ran' if sampler.ended_while_running.get(m['monitor_id']) else ''}")
        for n, (fa, cadence) in WORKERS.items():
            mine = [m for m in sampler.monitors.values() if m["job_id"] == job_of.get(n)]
            checks.append((f"{n} had a monitor on its job every {cadence} s",
                           any(m["every"] is not None and float(m["every"]) == cadence for m in mine)))
            hb = _read(f"{n}.hb")
            dead_at = float(hb) if hb else None
            alive_now = dead_at is not None and now - dead_at < 2.0
            if fa < 0:
                ran = (dead_at - float(starts[n])) if (dead_at and starts[n]) else 0.0
                info.append(f"{n}: ran {ran:.1f}s, no FATAL")
                checks.append((f"{n} ran ~the full minute before it was stopped ({ran:.0f}s)",
                               ran >= W60_MIN_RUN))
                checks.append((f"{n} is stopped at the end", dead_at is not None and not alive_now))
                checks.append((f"{n}'s monitor was CANCELLED while {n} still ran",
                               any(sampler.ended_while_running.get(m["monitor_id"]) for m in mine)))
                continue
            fatal = _read(f"{n}.fatal")
            if not fatal:
                checks.append((f"{n} printed its FATAL line", False))
                continue
            fatal_at, code = float(fatal.split()[0]), fatal.split()[1]
            lag = (dead_at - fatal_at) if dead_at else float("inf")
            info.append(f"{n}: FATAL code {code}, stopped {lag:.1f}s after it (cadence {cadence}s)")
            checks.append(((f"{n} stopped EARLY, after its FATAL and within {cadence}s + {SLACK:.0f}s "
                            f"({lag:.1f}s)"), not alive_now and 0 <= lag <= cadence + SLACK))
            checks.append((f"{n}'s FATAL line shown to the user (code {code})", code in transcript))
        checks.append((f"no job still running at the end ({len(sampler.running)})", not sampler.running))
        checks.append((f"no monitor still live at the end ({len(sampler.live)})", not sampler.live))

        for line in info:
            print(f"     {line}")
        if cut:
            print(f"     drive ended: {cut}")
        if settled:
            print(f"     settle: {settled}")
        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(allok, "parallel monitors stopped the FATAL workers early; the rest cancelled and stopped"
                if allok else "a monitor check failed")
    finally:
        if sampler is not None:
            sampler.stop.set()
        if sampling is not None and not sampling.done():
            await sampling
        await close()


if __name__ == "__main__":
    asyncio.run(main())
