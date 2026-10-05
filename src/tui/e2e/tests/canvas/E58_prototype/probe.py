"""E58 prototype — from an empty canvas an AI makes a script, the exec agent that
runs it and the UI wired both ways; then changes it. Scored by clicking."""

from __future__ import annotations

import asyncio
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, ask, say, wait_quiet  # noqa: E402

ASK = ("Prototype a ticker on this canvas, all by yourself: write a small script that prints an increasing counter, "
       "one line per second; make an exec agent that may run exactly that script; put a panel with Start and Stop "
       "buttons that start and stop it, and a live log panel that shows the script's lines as they arrive. "
       "No packaging, no build step — just make it work.")
CHANGE = "Change it: count by 10 instead, and show the last value big in its own panel."


def numbers(text: str) -> list[int]:
    return [int(n) for n in re.findall(r"(?<![\w.])(\d{1,7})(?![\w.])", text)]


def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(rig.plug.record())
    rig.init()
    t0 = time.time()
    from _lib.dev_surface import click_button, frame_eval, texts

    async def bodies(pg) -> list[str]:
        return [v.strip() for v in await frame_eval(pg, "document.body ? document.body.innerText : ''") if isinstance(v, str)]

    async def grows(pg, secs: float) -> bool:
        """The page's largest number grows by ≥ 2 within `secs`."""
        start = max(numbers(await texts(pg)), default=0)
        for _ in range(int(secs * 2)):
            await asyncio.sleep(0.5)
            if max(numbers(await texts(pg)), default=0) >= start + 2:
                return True
        return False

    async def still(pg) -> bool:
        """After a 3 s grace, the largest number holds for 5 s."""
        await asyncio.sleep(3)
        a = max(numbers(await texts(pg)), default=0)
        await asyncio.sleep(5)
        return max(numbers(await texts(pg)), default=0) == a

    async def by_ten(pg, secs: float) -> bool:
        for _ in range(int(secs * 2)):
            ns = {n for n in numbers(await texts(pg)) if n > 0 and n % 10 == 0}
            if any(n + 10 in ns and n + 20 in ns for n in ns):
                return True
            await asyncio.sleep(0.5)
        return False

    async def big(pg) -> bool:
        return any(re.fullmatch(r"\d+", b) and int(b) > 0 and int(b) % 10 == 0 for b in await bodies(pg))

    def made_script() -> list[str]:
        return [str(p.relative_to(rig.proj)) for p in rig.proj.rglob("*")
                if p.is_file() and ".fantastic" not in p.parts and p.stat().st_mtime >= t0]

    def exec_agents() -> list[dict]:
        import yaml
        out = []
        for f in (rig.proj / ".fantastic" / "agents").rglob("agent.yaml"):
            r = yaml.safe_load(f.read_text()) or {}
            if r.get("handler_module") == "exec_bridge.tools":
                out.append(r)
        return out

    async def drive():
        r: dict = {}
        async with rig.page(1900, 1100) as pg:
            done1, said1 = await ask(pg, 250, 300, ASK, rig.limits["total_seconds"], rig.limits["quiet_seconds"])
            await asyncio.sleep(3)
            scripts = made_script()
            names = [Path(s).name for s in scripts]
            execs = exec_agents()
            r["script_written"] = bool(scripts)
            # `allowed` is regexes (`ticker\.py`): unescape before looking for the name
            r["exec_runs_it"] = any(any(n in str(e.get("allowed", "")).replace("\\", "") for n in names) for e in execs)
            r["start_clicked"] = await click_button(pg, "start")
            r["counts"] = await grows(pg, 20)
            r["stop_clicked"] = await click_button(pg, "stop")
            # it STOPS only if it counted first — holding still never started is no stop
            r["stops"] = r["counts"] and await still(pg)
            await say(pg, CHANGE)
            done2 = await wait_quiet(pg, rig.limits["total_seconds"], rig.limits["quiet_seconds"])
            await asyncio.sleep(3)
            r["start2_clicked"] = await click_button(pg, "start")
            r["counts_by_10"] = await by_ten(pg, 30)
            r["big_value"] = await big(pg)
            await click_button(pg, "stop")
            return r, {"turn1_quiet": done1, "turn2_quiet": done2, "scripts": scripts,
                       "execs": [{"id": e.get("id"), "allowed": e.get("allowed")} for e in execs], "answer1": said1}
    try:
        checks, info = asyncio.run(drive())
    finally:
        rig.stop()
    rig.summary.update({"checks": checks, **info})
    print(f"info: {info}")
    ok = all(checks.values())
    return verdict(*rig.finish(ok, " ".join(f"{k}={'✓' if v else '✗'}" for k, v in checks.items())))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
