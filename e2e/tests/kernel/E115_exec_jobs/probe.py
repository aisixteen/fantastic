"""kernel/exec_jobs — exec_bridge jobs over REST, end to end. NO MODEL.

The kernel's own `exec_bridge` on the PACKAGED host of `RUNTIME` (python |
rust), every call through `web_rest`, its events watched over the `ws` face:

  1. the allowlist: an allow-listed command runs as a job; a line that matches no
     pattern is refused `unauthorized` and starts nothing; a record whose only
     pattern is a BROKEN regex permits nothing; `update_agent` refuses a broken one;
  2. `job_done` is exactly {type, job_id, exit_code, ok, lines, last_line} — no
     stdout/stderr — for a clean exit and a failing one;
  3. `tail`/`grep` on a job that printed > 256 KiB: the ring keeps the newest
     256 KiB of lines (bytes = len(line)+1), `dropped` counts the rest, `printed`
     counts all; `stream` filters stdout/stderr;
  4. a PTY job: `write` reaches it, `tail` and `output` read it, `job_done` is
     exactly {type, job_id, exit_code, ok}, `status` carries cols/rows;
  5. kernel `cancel task_id=<job_id>` on a running job: `ps` listed it, the OS
     process is gone, the row settles `stopped` and leaves `ps`;
  6. `status` shape: one job (with `cwd`) and the summary (without).

Every job is THIS interpreter running a `-c` program (absolute path, so the child
needs no PATH; output written as bytes, so no OS rewrites a line ending) — the same
fixture on every OS.
"""

from __future__ import annotations

import asyncio
import os
import re
import shlex
import shutil
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
from helpers.host.ws import WSWatcher
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
PY = sys.executable
# The allowlist matches the SHELL-JOINED line, where the interpreter's path is
# quoted as python's `shlex.quote` quotes it.
PY_LINE = re.escape(shlex.quote(PY))
ALLOWED = [PY_LINE + r" -c .*"]
ENV = oslayer.child_env()
GRANTS = {"$self": "*", "$parent": "*", "ws": "*"}
# 20000 lines of 32 chars: 33 ring bytes each (line + 1) → 7943 fit in 256 KiB.
BIG_N, BIG_W = 20000, 32
RING = 256 * 1024
KEPT = RING // (BIG_W + 1)
BIG = (
    "import sys; o = sys.stdout.buffer\n"
    f"for i in range(1, {BIG_N} + 1): o.write(b'line %06d xxxxxxxxxxxxxxxxxxxx\\n' % i)"
)
ONE_TWO = "import sys; sys.stdout.buffer.write(b'one\\ntwo\\n')"
DONE_KEYS = {"type", "job_id", "exit_code", "ok", "lines", "last_line"}
PTY_DONE_KEYS = {"type", "job_id", "exit_code", "ok"}
SNAP_KEYS = {"job_id", "status", "exit_code", "started_at", "lines", "last_line",
             "command", "args", "pty"}


#: A terminal's control sequences — CSI (`ESC [ … final`) and OSC (`ESC ] … BEL|ST`):
#: a console (ConPTY) writes them around a program's text where a unix tty
#: writes none.
_ESCAPES = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")


def plain(line: str) -> str:
    """A PTY line as the program wrote it: escapes and the trailing CR off."""
    return _ESCAPES.sub("", line).rstrip("\r").strip()


def watch(port: int, aid: str) -> list:
    """Collect `aid`'s events over the ws face, in a daemon thread, for the run."""
    seen: list = []

    async def run():
        w = WSWatcher(port, aid)
        await w.connect()
        async for ev in w.events():
            seen.append(ev)

    threading.Thread(target=lambda: asyncio.run(run()), daemon=True).start()
    return seen


def poll(fn, what: str, timeout: float = 30.0, step: float = 0.05):
    """fn() until truthy, bounded; returns the last value (falsy on timeout)."""
    end = time.monotonic() + timeout
    while True:
        v = fn()
        if v or time.monotonic() > end:
            if not v:
                print(f"  .. timed out waiting for {what}", flush=True)
            return v
        time.sleep(step)


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    work = Path(tempfile.mkdtemp(prefix="e2e-exec-jobs-"))
    run_dir = work / "run"
    run_dir.mkdir()
    # A record carrying a BROKEN regex can only arrive from disk (update_agent
    # refuses one): seeded under the rest leg before the kernel boots.
    broken = work / ".fantastic/agents/web/agents/rest/agents/ex_broken"
    broken.mkdir(parents=True)
    (broken / "agent.yaml").write_text(yaml.safe_dump({
        "id": "ex_broken", "handler_module": "exec_bridge.tools", "parent_id": "rest",
        "ingress_rule": "allow_all_ingress", "cwd": str(run_dir), "env": ENV,
        "allowed": [PY_LINE + " -c (.*"], "callable_by": GRANTS,
    }, sort_keys=False))
    hk = HostKernel(spec, work, reach=["ex", "ex_broken"]).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def ex(payload: dict, target: str = "ex") -> dict:
        r = hk.send(target, payload, timeout=60)
        return r if isinstance(r, dict) else {"error": f"non-dict reply {r!r}"}

    def start(program: str, **kw) -> dict:
        return ex({"type": "start", "command": PY, "args": ["-c", program], **kw})

    def status(jid: int) -> dict:
        jobs = ex({"type": "status", "job_id": jid}).get("jobs") or [{}]
        return jobs[0]

    def done_event(jid: int):
        return next((e for e in list(seen) if e.get("type") == "job_done"
                     and e.get("job_id") == jid), None)

    seen: list = []
    try:
        rec = hk.create(parent_id="rest", handler_module="exec_bridge.tools", id="ex",
                        ingress_rule="allow_all_ingress", cwd=str(run_dir), env=ENV,
                        allowed=ALLOWED, callable_by=GRANTS)
        need(rec.get("id") == "ex", f"create ex: {str(rec)[:200]}")
        seen = watch(hk.port, "ex")

        # 1 · the allowlist. The first job doubles as the watcher's barrier: its
        # job_done arriving proves the ws watch is live.
        s = start(ONE_TWO)
        jid = s.get("job_id")
        need(isinstance(jid, int) and not isinstance(jid, bool)
             and s.get("status") == "running" and s.get("pty") is False,
             f"an allow-listed line starts a job: {s}")
        ev = poll(lambda: done_event(jid), "job_done of the first job", timeout=5)
        if ev is None:
            # The watch may have registered after the job ended: one more job.
            jid = start(ONE_TWO).get("job_id")
            ev = poll(lambda: done_event(jid), "job_done (retry after watch)")
        need(ev is not None, "job_done arrives on the agent's event stream")
        ev = ev or {}
        need(set(ev) == DONE_KEYS, f"job_done keys exactly {sorted(DONE_KEYS)}: {sorted(ev)}")
        need(ev.get("exit_code") == 0 and ev.get("ok") is True and ev.get("lines") == 2
             and ev.get("last_line") == "two", f"job_done values: {ev}")
        need("stdout" not in ev and "stderr" not in ev, "job_done carries no stdout/stderr")

        before = len(ex({"type": "status"}).get("jobs") or [])
        r = ex({"type": "start", "command": PY, "args": ["-V"]})
        need(r.get("reason") == "unauthorized" and "job_id" not in r,
             f"a line no pattern matches is refused unauthorized: {str(r)[:200]}")
        after = len(ex({"type": "status"}).get("jobs") or [])
        need(after == before, f"the refused line started nothing ({before} → {after} jobs)")
        r = ex({"type": "start", "command": f"{PY} -c print(1)"})
        need(r.get("reason") == "unauthorized",
             f"a whole line in `command` is one word, refused: {str(r)[:160]}")

        r = ex({"type": "start", "command": PY, "args": ["-c", "print(1)"]}, target="ex_broken")
        need(r.get("reason") == "unauthorized" and "job_id" not in r,
             f"a broken `allowed` regex permits nothing: {str(r)[:200]}")
        r = hk.send(root, {"type": "update_agent", "id": "ex", "allowed": [PY_LINE + " (.*"]})
        need(bool(r.get("error")) and r.get("updated") is not True,
             f"update_agent refuses a broken regex: {str(r)[:200]}")
        r = start("print('still')")
        need(isinstance(r.get("job_id"), int),
             f"the refused update left the allowlist as it was: {str(r)[:160]}")

        # 2 · a failing job: exit code, ok, stderr counted as lines
        j2 = start("import sys\n"
                   "sys.stdout.buffer.write(b'out1\\n'); sys.stdout.flush()\n"
                   "sys.stderr.buffer.write(b'err1\\n'); sys.stderr.flush()\n"
                   "sys.stdout.buffer.write(b'out2\\n'); sys.exit(3)").get("job_id")
        ev2 = poll(lambda: done_event(j2), "job_done of the failing job") or {}
        need(set(ev2) == DONE_KEYS and ev2.get("exit_code") == 3 and ev2.get("ok") is False
             and ev2.get("lines") == 3,
             f"a failing job's job_done: {ev2}")
        t = ex({"type": "tail", "job_id": j2, "stream": "stderr"})
        need([x.get("line") for x in t.get("lines") or []] == ["err1"]
             and all(x.get("stream") == "stderr" for x in t.get("lines") or []),
             f"tail stream=stderr: {t.get('lines')}")
        t = ex({"type": "tail", "job_id": j2, "stream": "stdout"})
        need([x.get("line") for x in t.get("lines") or []] == ["out1", "out2"],
             f"tail stream=stdout: {t.get('lines')}")
        need(status(j2).get("status") == "failed", f"status of a failing job: {status(j2)}")

        # 3 · > 256 KiB of output: the ring
        jb = start(BIG).get("job_id")
        st = poll(lambda: (s := status(jb)).get("status") != "running" and s,
                  "the big job to end", timeout=120) or {}
        need(st.get("status") == "done" and st.get("lines") == BIG_N,
             f"the big job ends with {BIG_N} lines: {str(st)[:200]}")
        evb = poll(lambda: done_event(jb), "job_done of the big job", timeout=60) or {}
        need(evb.get("lines") == BIG_N and set(evb) == DONE_KEYS,
             f"big job_done: { {k: evb.get(k) for k in ('lines', 'exit_code')} }")
        t = ex({"type": "tail", "job_id": jb, "lines": 3})
        tl = t.get("lines") or []
        need([x.get("n") for x in tl] == [BIG_N - 2, BIG_N - 1, BIG_N]
             and tl[-1].get("line") == f"line {BIG_N:06d} xxxxxxxxxxxxxxxxxxxx"
             and tl[-1].get("stream") == "stdout",
             f"tail reads the newest lines: {tl}")
        need(t.get("printed") == BIG_N and t.get("dropped") == BIG_N - KEPT,
             f"tail: printed={t.get('printed')} dropped={t.get('dropped')} "
             f"(want {BIG_N}, {BIG_N - KEPT})")
        need(set(t) >= {"job_id", "status", "lines", "printed", "dropped"},
             f"tail shape: {sorted(t)}")
        first_kept = BIG_N - KEPT + 1
        g = ex({"type": "grep", "job_id": jb, "match": f"^line {first_kept:06d} "})
        need(g.get("total") == 1 and (g.get("matches") or [{}])[0].get("n") == first_kept,
             f"grep finds the oldest kept line {first_kept}: {str(g)[:200]}")
        g = ex({"type": "grep", "job_id": jb, "match": f"^line {first_kept - 1:06d} "})
        need(g.get("total") == 0, f"grep cannot find a dropped line: total={g.get('total')}")
        g = ex({"type": "grep", "job_id": jb, "match": r"^line 0199[0-9]{2} ", "lines": 5})
        need(g.get("total") == 100 and [m.get("n") for m in g.get("matches") or []]
             == list(range(19995, 20000))
             and g.get("scanned") == KEPT and g.get("dropped") == BIG_N - KEPT,
             f"grep: total={g.get('total')} scanned={g.get('scanned')} "
             f"dropped={g.get('dropped')} last={[m.get('n') for m in g.get('matches') or []]}")
        r = ex({"type": "tail", "job_id": jb, "lines": 501})
        need(r.get("reason") == "bad_arg", f"tail lines > 500 refused: {str(r)[:120]}")
        r = ex({"type": "grep", "job_id": jb, "match": "("})
        need(r.get("reason") == "bad_arg", f"grep with a broken regex refused: {str(r)[:120]}")

        # 4 · a PTY job
        sp = start("x = input(); print('got:' + x)", pty={"cols": 100, "rows": 30})
        jp = sp.get("job_id")
        need(isinstance(jp, int) and sp.get("pty") is True, f"a PTY start: {sp}")
        sj = status(jp)
        need(sj.get("pty") is True and sj.get("cols") == 100 and sj.get("rows") == 30,
             f"PTY status carries cols/rows: {sj}")
        w = ex({"type": "write", "job_id": jp, "data": "zz\r"})
        need(not w.get("error"), f"write to the PTY: {w}")
        evp = poll(lambda: done_event(jp), "job_done of the PTY job") or {}
        need(set(evp) == PTY_DONE_KEYS and evp.get("exit_code") == 0 and evp.get("ok") is True,
             f"PTY job_done exactly {sorted(PTY_DONE_KEYS)}: {evp}")
        t = ex({"type": "tail", "job_id": jp})
        need(any(plain(x.get("line", "")) == "got:zz" for x in t.get("lines") or [])
             and all(x.get("stream") == "pty" for x in t.get("lines") or []),
             f"tail on a PTY job: {t.get('lines')}")
        o = ex({"type": "output", "job_id": jp})
        need(isinstance(o.get("output"), str) and "got:zz" in o["output"],
             f"output replays the PTY ring: {str(o)[:160]}")

        # 5 · cancel a running job
        jc = start("import os, sys, time\n"
                   "sys.stdout.buffer.write(b'%d\\n' % os.getpid()); sys.stdout.flush()\n"
                   "time.sleep(60)").get("job_id")
        pid_line = poll(lambda: (ex({"type": "tail", "job_id": jc}).get("lines") or [None])[0],
                        "the sleeper's pid")
        pid = int(pid_line["line"]) if pid_line and pid_line.get("line", "").isdigit() else None
        need(pid is not None, f"the sleeper printed its pid: {pid_line}")
        ps = hk.send("kernel", {"type": "ps"})
        need(any(t.get("id") == jc for t in ps.get("tasks") or []),
             f"ps lists the running job {jc}")
        c = hk.send("kernel", {"type": "cancel", "task_id": jc})
        need(c.get("cancelled") is True and jc in (c.get("killed") or []),
             f"cancel task_id=<job_id>: {c}")
        sc = poll(lambda: (s := status(jc)).get("status") == "stopped" and s,
                  "the cancelled row to settle") or status(jc)
        need(sc.get("status") == "stopped", f"the cancelled job settles stopped: {sc}")

        def gone() -> bool:
            return not oslayer.pid_alive(pid)

        need(pid is not None and bool(poll(gone, "the OS process to die", timeout=10)),
             f"the OS process {pid} is gone")
        need(not any(t.get("id") == jc for t in hk.send("kernel", {"type": "ps"}).get("tasks") or []),
             "the cancelled job left ps")

        # 6 · status shape
        one = ex({"type": "status", "job_id": jid})
        oj = (one.get("jobs") or [{}])[0]
        need(set(oj) == SNAP_KEYS | {"cwd"} and oj.get("cwd") == str(run_dir)
             and oj.get("command") == PY and oj.get("args") == ["-c", ONE_TWO]
             and oj.get("status") == "done" and oj.get("exit_code") == 0
             and isinstance(oj.get("started_at"), (int, float)),
             f"status job_id=: {oj}")
        allj = ex({"type": "status"}).get("jobs") or []
        need(len(allj) >= 6 and all(set(j) == SNAP_KEYS for j in allj if not j.get("pty"))
             and all(set(j) == SNAP_KEYS | {"cols", "rows"} for j in allj if j.get("pty")),
             f"status summary: {len(allj)} jobs, keys {sorted({k for j in allj for k in j})}")
        r = ex({"type": "status", "job_id": "1"})
        need(r.get("reason") == "bad_arg", f"a string job_id is refused, never coerced: {str(r)[:120]}")
        r = ex({"type": "status", "job_id": 999999999})
        need(r.get("reason") == "not_found", f"an unknown job_id: {str(r)[:120]}")
    finally:
        hk.close()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
