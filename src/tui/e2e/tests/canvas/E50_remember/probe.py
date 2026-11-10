"""E50 remember — a dialog survives a closed cell and a host restart, and is reopened to recall."""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, answer, open_chat, say, verdict, wait_quiet  # noqa: E402


def main() -> bool:
    rig = Rig(__file__)
    why = rig.plug_or_refusal()
    if why:
        return verdict(*rig.finish(False, why))
    rig.build()
    rig.make_home(rig.plug.record())

    rig.init()

    async def first() -> str:
        async with rig.page() as pg:
            ai = await open_chat(pg, 250, 300)
            await say(pg, "Remember this: my build code is ZEBRA-42. Just say OK.")
            await wait_quiet(pg, rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
            return ai

    async def second(ai: str) -> str:
        from canvas import host_call

        async with rig.page() as pg:
            # the dialog's agent lives on; the sidebar reopens it as `place {backend_id}`
            placed = await host_call(rig.port, f"{rig.page_guid()}|view_root",
                                     {"type": "place", "backend_id": ai, "x": 900, "y": 150, "width": 440, "height": 520})
            if "placed" not in placed:
                raise SystemExit(f"the dialog was not reopened: {placed}")
            await pg.until("document.querySelectorAll('.agent-frame iframe').length > 0", 20)
            await asyncio.sleep(2)
            await say(pg, "What is my build code?")
            await wait_quiet(pg, rig.budget(rig.limits["total_seconds"]), rig.limits["quiet_seconds"])
            return await answer(pg)

    try:
        ai = asyncio.run(first())
        rig.stop()
        rig.start()
        said = asyncio.run(second(ai))
    finally:
        rig.stop()
    rig.summary.update({"answer": said})
    print(f"recalled: {said[:300]!r}")
    ok = "ZEBRA-42" in (said or "")
    return verdict(*rig.finish(ok, f"recalled after restart + reopen: {ok}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
