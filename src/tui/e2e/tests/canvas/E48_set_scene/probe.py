"""E48 set_scene — the background singleton, as dara's sunset / unified_vis's motif."""

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

    rig.init()

    async def drive():
        async with rig.page() as pg:
            r = await ask(pg, 250, 300, "Make the canvas background a slow blue gradient.",
                          rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
            await asyncio.sleep(2)
            return r, pg.logs
    try:
        (done, said), logs = asyncio.run(drive())
    finally:
        rig.stop()
    bg = (rig.host_record("background") or {})
    src = bg.get("bg_source", "") or ""
    syntax = [e for e in logs if "SyntaxError" in e or "ReferenceError" in e]
    rig.summary.update({"answer": said, "bg_source": src, "errors": syntax})
    print(f"bg_source ({len(src)} chars): {src[:300]!r} · still locked: {bg.get('delete_lock')}")
    ok = len(src) > 0 and not syntax and bg.get("delete_lock") is True
    return verdict(*rig.finish(ok, f"background set ({len(src)} chars), no script error, still delete-locked: {ok}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
