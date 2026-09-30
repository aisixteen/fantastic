"""E49 terminal_task — a real task through the canvas terminal."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, answer, ask, html_cell, http_get, in_frame, say, spot, wait_quiet  # noqa: E402,F401

def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(rig.plug.record())

    import subprocess
    rig.init()
    g = lambda *a: subprocess.run(["git", *a], cwd=rig.proj, capture_output=True, check=True)
    g("init", "-q"); g("config", "user.email", "t@t"); g("config", "user.name", "t")
    (rig.proj / ".gitignore").write_text(".fantastic/\n")
    for i, msg in enumerate(["first: start", "second: add notes", "third: add readme"]):
        (rig.proj / f"f{i}.txt").write_text(msg)
        g("add", "-A"); g("commit", "-q", "-m", msg)

    async def drive():
        async with rig.page() as pg:
            await spot(pg, 1100, 250, "terminal")
            await pg.until("!!document.querySelector('.agent-frame .xterm')", 20)
            await asyncio.sleep(2)
            return await ask(pg, 250, 300,
                "Use the terminal next to you: how many commits does this repo have, and what is the last commit message?",
                rig.limits["total_seconds"], rig.limits["quiet_seconds"])
    try:
        done, said = asyncio.run(drive())
    finally:
        rig.stop()
    rig.summary.update({"answer": said})
    print(f"answer: {said[:400]}")
    ok = bool(re.search(r"(?<!\d)3(?!\d)|three", said, re.I)) and "third" in said.lower()
    return verdict(*rig.finish(ok, f"3 commits and 'third: add readme' named: {ok}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
