"""E50 remember — keep → close → restart → reopen → recall."""

from __future__ import annotations

import asyncio
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict, open_sidebar, answer, ask, html_cell, http_get, in_frame, say, spot, wait_quiet  # noqa: E402,F401

def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(rig.plug.record())

    rig.init()

    async def first():
        async with rig.page() as pg:
            await ask(pg, 250, 300, "Remember this: my build code is ZEBRA-42. Just say OK.",
                      rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
            await pg.js("document.querySelector('.chat-view .c-keep').click()")
            await asyncio.sleep(2)
            kept = await pg.js("document.querySelector('.chat-view .c-badge')?.textContent")
            await pg.js("[...document.querySelectorAll('.agent-frame')].find(f=>f.querySelector('.chat-view')).querySelector('.close').click()")
            await asyncio.sleep(2)
            return kept

    async def second():
        async with rig.page() as pg:
            # the kept dialog is a tile in the double-click (`host · chats`)
            for c in (1, 2):
                await pg.mouse("mousePressed", 900, 150, c)
                await pg.mouse("mouseReleased", 900, 150, c)
            await pg.until("document.querySelectorAll('.action-popup .tile[data-kind=chat]').length > 0", 20)
            await pg.js("document.querySelector('.action-popup .tile[data-kind=chat]').dispatchEvent(new MouseEvent('mousedown',{bubbles:true}))")
            await pg.until("!!document.querySelector('.chat-view')", 20)
            await asyncio.sleep(2)
            await say(pg, "What is my build code?")
            await wait_quiet(pg, rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
            return await answer(pg)
    try:
        kept = asyncio.run(first())
        rig.stop()
        rig.start()
        said = asyncio.run(second())
    finally:
        rig.stop()
    rig.summary.update({"kept_badge": kept, "answer": said})
    print(f"kept: {kept!r} · recalled: {said[:300]!r}")
    ok = "ZEBRA-42" in (said or "")
    return verdict(*rig.finish(ok, f"recalled after close + restart: {ok}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
