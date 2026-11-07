"""E93 reflect_view — a host bundle agent's html view is drawn from its reflect (#483)."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402
from _lib.dev_surface import frame_eval  # noqa: E402

ID = "e92_drawn"


def body(text: str) -> str:
    return f"<p id='e92'>{text}</p>"


async def drawn(pg, text: str, secs: float = 20) -> bool:
    """Whether some frame of the page shows `text` in the cell's `#e92`."""
    expr = "(document.getElementById('e92')?.textContent||'')"
    for _ in range(int(secs * 5)):
        if any(v == text for v in await frame_eval(pg, expr)):
            return True
        await asyncio.sleep(0.2)
    return False


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.init()
    r: dict = {}
    info: dict = {}

    async def run() -> None:
        from canvas import host_call

        guid = rig.page_guid()
        made = await host_call(rig.port, "kernel", {
            "type": "create_agent", "parent_id": "user", "id": ID, "handler_module": "ram_store.tools",
            "callable_by": {"user": ["reflect", "watch"]}, "view": {"kind": "html", "bytes": body("from reflect one"), "calls": ["reflect"]}})
        r["0 the agent is made"] = "error" not in made
        reflected = await host_call(rig.port, ID, {"type": "reflect", "tree": "none"})
        info["reflect_view"] = reflected.get("view")
        r["1 its reflect carries the view"] = (reflected.get("view") or {}).get("bytes") == body("from reflect one")
        async with rig.page(1200, 800) as pg:
            placed = await host_call(rig.port, f"{guid}|view_root", {"type": "place", "backend_id": ID, "x": 40, "y": 40, "width": 420, "height": 240})
            r["2 it is placed"] = "placed" in placed
            r["3 the cell shows the body its reflect carries"] = await drawn(pg, "from reflect one", 30)
            await host_call(rig.port, "kernel", {"type": "update_agent", "id": ID, "view": {"kind": "html", "bytes": body("from reflect two"), "calls": ["reflect"]}})
            r["4 a changed view redraws the cell"] = await drawn(pg, "from reflect two", 30)
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["5 no page errors"] = not info["page_errors"]

    try:
        asyncio.run(run())
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:3000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
