"""kernel/drain_lock — one kernel per directory, and a graceful stop over REST.
NO MODEL.

The PACKAGED host of `RUNTIME` (python | rust | swift), every call over `web_rest`:

  1. THE LOCK: `.fantastic/lock.yaml` names the serving kernel's pid; a second
     kernel started in the same directory exits non-zero naming that pid, and the
     first keeps serving;
  2. THE DRAIN (`shutdown_agents`) with a call in flight: new work is refused
     `{reason:"draining"}`, the root's control surface (`ps`, `reflect`, `ledger`)
     still answers, the call in flight is cancelled after the quiet window and its
     REST caller gets the settlement, the ledger holds it as a dead letter that
     `ack_deadletter` retires; work stays refused after the drain;
  3. `kernel_stop`: acked `{type:kernel_stop, ok:true}`, the process exits 0, the
     port closes, the lock releases (the next boot of the directory serves);
  4. A STALE LOCK — a `lock.yaml` naming a DEAD pid — does not block a boot, nor
     does one naming a LIVE pid that holds no lock (this probe's own);
  5. `kernel_stop` with a call in flight and no drain before it: the call's REST
     caller still gets its settlement, and the process exits 0.
"""

from __future__ import annotations

import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import yaml

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers import oslayer
from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
CB = {"$self": "*", "$parent": "*"}
#: A drain waits out the kernel's quiet window (10 s in python) before it
#: cancels; every wait below is bounded well past it.
BOUND = 60.0


class Bg:
    """One REST call on a thread — the caller of a call that is in flight."""

    def __init__(self, hk: HostKernel, target: str, payload: dict):
        self.out: dict | None = None
        self.t = threading.Thread(target=self._run, args=(hk, target, payload), daemon=True)
        self.t.start()

    def _run(self, hk, target, payload):
        try:
            self.out = hk.send(target, payload, timeout=BOUND)
        except Exception as e:  # noqa: BLE001 — a dropped connection is the evidence
            self.out = {"transport_error": f"{type(e).__name__}: {e}"}

    def join(self) -> dict | None:
        self.t.join(BOUND)
        return self.out


def poll(fn, bound: float = 10.0, step: float = 0.05):
    """`fn()` until truthy, bounded; the last value either way."""
    end = time.monotonic() + bound
    v = fn()
    while not v and time.monotonic() < end:
        time.sleep(step)
        v = fn()
    return v


def port_open(port: int) -> bool:
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


def lock_pid(work: Path):
    p = work / ".fantastic" / "lock.yaml"
    if not p.exists():
        return None
    doc = yaml.safe_load(p.read_text()) or {}
    return doc.get("pid") if isinstance(doc, dict) else None


def dead_pid() -> int:
    """A pid that WAS a process and is not one now."""
    p = subprocess.Popen([sys.executable, "-c", ""])
    p.wait()
    return p.pid


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-drain-"))
    bad: list[str] = []
    kernels: list[HostKernel] = []
    stray: list[subprocess.Popen] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def boot(tag: str) -> HostKernel:
        hk = HostKernel(spec, work, reach=["slow", "w"]).boot()
        kernels.append(hk)
        need(hk.send("kernel", {"type": "reflect", "tree": "none"}).get("guid") is not None,
             f"{tag}: the kernel boots and answers")
        return hk

    def in_flight(hk: HostKernel) -> tuple[Bg, int | None]:
        """Start a `slow` call (never finishes by itself); its task id once running."""
        before = hk.send("slow", {"type": "started"}).get("started", 0)
        bg = Bg(hk, "slow", {"type": "slow"})
        poll(lambda: hk.send("slow", {"type": "started"}).get("started", 0) > before)
        tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
        tid = next((t["id"] for t in tasks if t.get("agent") == "slow" and t.get("verb") == "slow"), None)
        return bg, tid

    def stopped(hk: HostKernel, tag: str) -> None:
        try:
            rc = hk.proc.wait(BOUND)
        except subprocess.TimeoutExpired:
            rc = None
        need(rc == 0, f"{tag}: the process exits 0 (got {rc})")
        need(not port_open(hk.port), f"{tag}: the port is closed")

    try:
        # 1 · the lock
        hk = boot("first boot")
        for aid in ("slow", "w"):
            hk.create(parent_id="rest", handler_module="slow_probe.tools", id=aid, callable_by=CB)
        need(lock_pid(work) == hk.proc.pid, f"lock.yaml names the serving pid ({lock_pid(work)} vs {hk.proc.pid})")
        log = work.parent / f"{work.name}-second.out"
        with log.open("wb") as f:
            second = subprocess.Popen(hk.cmd, cwd=work, env=hk.env, stdout=f,
                                      stderr=subprocess.STDOUT, **oslayer.group_kwargs())
        stray.append(second)
        try:
            rc = second.wait(30)
        except subprocess.TimeoutExpired:
            rc = None
        said = log.read_text(errors="replace")
        log.unlink(missing_ok=True)
        need(rc not in (None, 0), f"a second kernel in the same dir exits non-zero (got {rc})")
        need(str(hk.proc.pid) in said, f"it names the owner's pid: {said.strip()[:200]!r}")
        need(hk.send("w", {"type": "work"}).get("runs") == 1, "the first kernel still serves")
        need(lock_pid(work) == hk.proc.pid, "lock.yaml still names the first kernel")

        # 2 · the drain, with a call in flight
        bg, tid = in_flight(hk)
        need(tid is not None, f"a slow call is in flight (task {tid})")
        drain = Bg(hk, "kernel", {"type": "shutdown_agents"})
        refused = poll(lambda: hk.send("w", {"type": "work"}).get("reason") == "draining")
        need(bool(refused), "while draining, new work is refused {reason: draining}")
        ps = hk.send("kernel", {"type": "ps"}).get("tasks") or []
        need(any(t.get("id") == tid for t in ps), f"while draining, `ps` answers and lists the call in flight ({len(ps)} tasks)")
        rf = hk.send("kernel", {"type": "reflect", "tree": "none"})
        need(rf.get("guid") is not None, f"while draining, the root's reflect answers: {str(rf)[:120]}")
        d = drain.join()
        need(isinstance(d, dict) and d.get("drained") is True and tid in (d.get("cancelled") or []),
             f"shutdown_agents → drained, the call in flight cancelled: {d}")
        out = bg.join()
        need(out == {"cancelled": True}, f"the in-flight call's REST caller gets the settlement: {out}")
        after = hk.send("w", {"type": "work"})
        need(after.get("reason") == "draining", f"after the drain, work is still refused: {str(after)[:160]}")
        led = hk.send("kernel", {"type": "ledger"}).get("executions") or []
        dl = next((e for e in led if e.get("target") == "slow" and e.get("status") == "cancelled"), None)
        need(dl is not None and not dl.get("acked"), f"the ledger holds it as an unacked dead letter: {dl}")
        if dl is not None:
            a = hk.send("kernel", {"type": "ack_deadletter", "id": dl["id"]})
            need(a.get("acked") is True, f"ack_deadletter retires it, draining or not: {a}")

        # 3 · kernel_stop
        st = hk.send("kernel", {"type": "kernel_stop"})
        need(st == {"type": "kernel_stop", "ok": True}, f"kernel_stop is acked: {st}")
        stopped(hk, "kernel_stop after the drain")
        need(lock_pid(work) != hk.proc.pid, f"the lock no longer names the stopped pid ({lock_pid(work)})")

        # 4 · stale locks
        gone = dead_pid()
        (work / ".fantastic" / "lock.yaml").write_text(yaml.safe_dump({"pid": gone}))
        hk = boot(f"a lock naming a dead pid ({gone})")
        need(lock_pid(work) == hk.proc.pid, f"the new kernel takes the lock ({lock_pid(work)})")
        need(hk.send("w", {"type": "work"}).get("runs") == 1, "it serves new work")
        # 5 · kernel_stop with a call in flight, no drain before it
        bg, tid = in_flight(hk)
        need(tid is not None, f"a slow call is in flight (task {tid})")
        st = hk.send("kernel", {"type": "kernel_stop"})
        need(st == {"type": "kernel_stop", "ok": True}, f"kernel_stop is acked with a call in flight: {st}")
        out = bg.join()
        need(out == {"cancelled": True}, f"the in-flight call's REST caller gets the settlement: {out}")
        stopped(hk, "kernel_stop with a call in flight")

        (work / ".fantastic" / "lock.yaml").write_text(yaml.safe_dump({"pid": os.getpid()}))
        hk = boot(f"a lock naming a live pid that holds no lock ({os.getpid()})")
        need(lock_pid(work) == hk.proc.pid, f"the new kernel takes the lock ({lock_pid(work)})")
    finally:
        for k in kernels:
            k.close()
        for p in stray:
            if p.poll() is None:
                oslayer.kill_group(p.pid)
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
