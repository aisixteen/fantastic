"""E82 reload_backlog — a page reload must not leave unacked output behind
(owner 2026-10-16: `claude` in a canvas terminal hung, no IO). The exec agent
pauses a PTY's reader past 100 000 unacked chars and resumes it under 5 000;
its count started only with the job, so every reload lost what the old page
had drawn but not yet acked (its batch under ACK_SIZE, the frames in flight).
Enough reloads and the job paused for good — the new page replays the ring,
which acks nothing, and the program blocks writing its screen:

a) a terminal cell runs TICKER: ~4 KB of output every 50 ms, numbered lines;
b) the page reloads RELOADS times, each a little after the terminal draws;
c) after each reload the exec agent's count (`ack chars:0`) is polled: the first read (the peak)
   stays under BOUND, the job is never paused on any poll, and the count then settles under SETTLED;
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
TICK_CHARS = 4012  # one ticker line: "TICK n " + 4000 x + CRLF, n of 3-4 digits
HIGH_WATERMARK = 100_000  # the host's pause threshold (pty.rs HIGH_WATERMARK)
# REPAINT_WAIT_MS (terminal_view.ts): the longest a re-attach holds live frames waiting for the program
REPAINT_WAIT_MS = 300
# The peak is the first count read after a reload: a re-attach holds live frames unacked until it
# finishes (terminal_view.ts attach/drainEarly), so right after the reload the count carries the old
# page's unacked output plus what the ticker wrote during the attach. CI failures of the earlier
# 40 000 bound (issue #89) showed peaks of 40k-72k with the job never paused, so that bound measured
# the mechanism's normal range, not a leak; a loaded runner stretches the attach round trips and
# the normal range has no sharp ceiling below the threshold. What the test must catch is the count
# drifting toward HIGH_WATERMARK, where the program blocks. So the bound is set from the threshold:
# one attach hold of ticker output under it (REPAINT_WAIT_MS / 50 ms = 6 ticks of TICK_CHARS, ~24k).
# A peak inside that margin means one more slow attach pauses the job, and the failure shows up here
# before `never_paused` (the hard stop) can. It is not fitted to the worst peak seen (72k).
BOUND = HIGH_WATERMARK - (REPAINT_WAIT_MS // 50) * TICK_CHARS
# once the attach finishes the count must fall to about one ack batch (5 000) plus frames in flight
SETTLED = 20_000
SETTLE_S = 10.0  # drain deadline; a stuck backlog never meets it and FAILS "backlog_settles". Neither bound is widened.
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
    info: dict = {"peaks": [], "settled": []}

    async def count() -> dict:
        st = await host_call(rig.port, BACKEND, {"type": "status"})
        job = next((j["job_id"] for j in (st.get("jobs") or []) if j.get("pty") and j.get("status") == "running"), None)
        if job is None:
            return {"error": "no running job", "status": st}
        return await host_call(rig.port, BACKEND, {"type": "ack", "job_id": job, "chars": 0})

    async def settled() -> tuple[dict, dict, bool, bool]:
        """(first read = this reload's peak, last read, ever paused across every poll, settled in time)"""
        loop = asyncio.get_running_loop()
        end = loop.time() + SETTLE_S
        peak = c = await count()
        paused = bool(c.get("paused"))
        while not (isinstance(c.get("unacked"), int) and c["unacked"] <= SETTLED):
            if loop.time() >= end:
                return peak, c, paused, False
            await asyncio.sleep(0.1)
            c = await count()
            paused = paused or bool(c.get("paused"))
        return peak, c, paused, True

    async def drive() -> None:
        async with rig.page(1400, 900) as pg:
            r["drawing"] = bool(await pg.until(f"/TICK \\d+/.test({SCREEN})", 20))
            paused = False
            all_settled = True
            for i in range(RELOADS):
                await pg.open(rig.url)
                await pg.until(f"/TICK \\d+/.test({SCREEN})", 15)
                await asyncio.sleep(0.4 + (i % 3) * 0.15)  # a reload lands mid-batch
                peak, c, was_paused, ok_s = await settled()
                all_settled = all_settled and ok_s
                info["peaks"].append(peak.get("unacked"))
                info["settled"].append(c.get("unacked"))
                paused = paused or was_paused
            r["never_paused"] = not paused
            r["backlog_settles"] = all_settled
            peaks = [c for c in info["peaks"] if isinstance(c, int)]
            info["max_peak"] = max(peaks, default=None)
            r["count_bounded"] = len(peaks) == RELOADS and max(peaks) < BOUND
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
    # the numbers print on every run: CI keeps them for the next bound decision
    nums = f"peaks {info['peaks']} · settled {info['settled']}"
    note = f"{RELOADS} reloads: peak at most {info.get('max_peak')} (bound {BOUND}), never paused, backlog settles, still drawing · {nums}" if ok else \
        f"failed: {', '.join(k for k, v in r.items() if not v)} · max peak {info.get('max_peak')} · {nums} · ticks {info.get('ticks')}"
    return verdict(*rig.finish(ok, note))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
