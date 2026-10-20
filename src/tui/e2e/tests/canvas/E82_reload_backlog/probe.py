"""E82 reload_backlog — a page reload must not leave unacked output behind
(owner 2026-10-16: `claude` in a canvas terminal hung, no IO). The exec agent
pauses a PTY's reader past 100 000 unacked chars and resumes it under 5 000;
its count started only with the job, so every reload lost what the old page
had drawn but not yet acked (its batch under ACK_SIZE, the frames in flight).
Enough reloads and the job paused for good — the new page replays the ring,
which acks nothing, and the program blocks writing its screen:

a) a terminal cell runs TICKER: ~4 KB of output every 50 ms, numbered lines;
b) the page reloads RELOADS times, each a little after the terminal draws;
c) after each re-attach settles, the exec agent's count (`ack chars:0`) is read:
   it stays under BOUND and the job is never paused;
d) after the last reload the terminal still draws new lines."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, verdict, py_job  # noqa: E402

TICKER = r'''import sys, time
n = 0
while True:
    n += 1
    sys.stdout.write(f"TICK {n} " + "x" * 4000 + "\r\n")
    sys.stdout.flush()
    time.sleep(0.05)
'''
CELL, BACKEND = "cell_tick", "exec_bridge_tick"
VERBS = ["watch", "status", "start", "write", "ack", "resize", "output"]
RELOADS = 30
# a fresh viewer's own count: at most one ack batch (5 000) plus what is in flight
BOUND = 40_000
# a re-attach holds live frames unacked until it finishes (terminal_view.ts attach/drainEarly), so the
# count is read once the attach has settled: it must fall to about one ack batch plus frames in flight
SETTLED = 20_000
SETTLE_S = 10.0  # drain deadline; a stuck backlog never meets it and FAILS the "settled" check. BOUND is not widened.
SCREEN = ("(()=>{const f=document.querySelector('.agent-frame[data-id=\"%s\"] .frame-body'); const t=f&&f.xterm; if(!t) return '';"
          " const b=t.buffer.active; let s=''; for(let i=0;i<b.length;i++){const l=b.getLine(i); if(l) s+=l.translateToString(true)+' ';} return s;})()" % CELL)


def last_tick(text: str) -> int:
    return max((int(m) for m in re.findall(r"TICK (\d+)", text or "")), default=0)


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "ticker.py").write_text(TICKER)
    cell = {"id": CELL, "handler_module": "terminal_view.ts", "backend_id": BACKEND, "owns_backend": False,
            "command": py_job("ticker.py")[0], "can_call": {"$self": "*", BACKEND: VERBS}, "callable_by": {"$self": "*"},
            "x": 40, "y": 40, "width": 600, "height": 300}
    host = {"id": BACKEND, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
            "ingress_rule": "allow_all_ingress", "allowed": [py_job("ticker.py")[1]], "cell_id": CELL, "can_call": {"$self": "*"},
            "callable_by": {"$self": "*", "mcp": ["*"], "user": ["*"], "canvas_root": ["*"], "canvas_ws": ["watch"]}}
    rig.init(cells=[cell], host_agents=[(FOLDER, host)])
    sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e" / "runners"))
    from canvas import host_call
    r: dict = {}
    info: dict = {"counts": []}

    async def count() -> dict:
        st = await host_call(rig.port, BACKEND, {"type": "status"})
        job = next((j["job_id"] for j in (st.get("jobs") or []) if j.get("pty") and j.get("status") == "running"), None)
        if job is None:
            return {"error": "no running job", "status": st}
        return await host_call(rig.port, BACKEND, {"type": "ack", "job_id": job, "chars": 0})

    async def settled() -> tuple[dict, bool]:
        loop = asyncio.get_running_loop()
        end = loop.time() + SETTLE_S
        c = await count()
        while not (isinstance(c.get("unacked"), int) and c["unacked"] <= SETTLED):
            if loop.time() >= end:
                return c, False
            await asyncio.sleep(0.1)
            c = await count()
        return c, True

    async def drive() -> None:
        async with rig.page(1400, 900) as pg:
            r["drawing"] = bool(await pg.until(f"/TICK \\d+/.test({SCREEN})", 20))
            paused = False
            all_settled = True
            for i in range(RELOADS):
                await pg.open(rig.url)
                await pg.until(f"/TICK \\d+/.test({SCREEN})", 15)
                await asyncio.sleep(0.4 + (i % 3) * 0.15)  # a reload lands mid-batch
                c, ok_s = await settled()
                all_settled = all_settled and ok_s
                info["counts"].append(c.get("unacked"))
                paused = paused or bool(c.get("paused"))
            r["never_paused"] = not paused
            r["backlog_settles"] = all_settled
            counts = [c for c in info["counts"] if isinstance(c, int)]
            info["max_count"] = max(counts, default=None)
            r["count_bounded"] = bool(counts) and max(counts) < BOUND
            a = last_tick(await pg.js(SCREEN))
            await asyncio.sleep(2.0)
            b = last_tick(await pg.js(SCREEN))
            info["ticks"] = [a, b]
            r["still_drawing"] = b > a
            info["page_errors"] = [e for e in pg.logs if "EXC" in e][:3]

    try:
        asyncio.run(drive())
    finally:
        rig.stop()
    ok = bool(r) and all(r.values())
    rig.summary["checks"], rig.summary["info"] = r, info
    note = f"{RELOADS} reloads: unacked at most {info.get('max_count')}, never paused, still drawing" if ok else \
        f"failed: {', '.join(k for k, v in r.items() if not v)} · max {info.get('max_count')} · counts {info['counts']} · ticks {info.get('ticks')}"
    return verdict(*rig.finish(ok, note))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
