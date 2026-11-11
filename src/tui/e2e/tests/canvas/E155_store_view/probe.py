"""E155 store_view — the canvas store view over a real `append` (#505, #508).

The view reads `dump` (`tables.rows`) and counts per owner; an `appended` event of the
store makes it read again, so rows appended by an agent show with no click."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402
from _lib.host_view import lines, shown, until_line  # noqa: E402

ID = "e155_store"
VIEW = "store"


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
            "callable_by": {"user": ["append", "dump", "search", "watch"]}})
        r["0a the store is made"] = "error" not in made
        async with rig.page(1200, 800) as pg:
            placed = await host_call(rig.port, f"{guid}|view_root", {"type": "place", "backend_id": ID, "x": 40, "y": 40, "width": 520, "height": 360})
            r["0b it is placed"] = "placed" in placed
            r["0c the view reads the empty store"] = await until_line(pg, VIEW, lambda t: t == "no rows yet", 30)
            got = await host_call(rig.port, ID, {"type": "append", "rows": [{"text": "first e155"}, {"text": "second e155"}]})
            info["append"] = got
            r["1 a real append answers appended: 2"] = got.get("appended") == 2 and len(got.get("ids") or []) == 2
            dump = await host_call(rig.port, ID, {"type": "dump"})
            rows = ((dump.get("tables") or {}).get("rows")) or []
            owners = sorted({x.get("owner") for x in rows if isinstance(x, dict)})
            info["dump_owners"], info["dump_rows"] = owners, len(rows)
            r["2 dump answers the rows under tables.rows, each with its owner"] = len(rows) == 2 and len(owners) == 1 and bool(owners[0])
            owner = owners[0] if owners else "?"
            r["3a the view shows the rows with no click"] = await until_line(pg, VIEW, lambda t: t == "OWNERS · 1 · 2 rows", 30)
            r["3b the owner's line counts them"] = await until_line(pg, VIEW, lambda t: t == f"{owner}  2 rows  0 folds", 10)
            await host_call(rig.port, ID, {"type": "append", "rows": [{"text": "third e155"}]})
            r["4 a second append shows by itself"] = await until_line(pg, VIEW, lambda t: t == "OWNERS · 1 · 3 rows", 30)
            shown(info, "view_rows", await lines(pg, VIEW))
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["5 no page error"] = not info["page_errors"]

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
