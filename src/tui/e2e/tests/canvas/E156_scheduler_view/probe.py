"""E156 scheduler_view — the canvas scheduler view over a real `scheduler.tools` (#507).

Create, schedule, tick, unschedule on the fantastic binary's own scheduler; the view
lists the job and its last result, and a fire reaches it through `schedule_fired`."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402
from _lib.host_view import lines, refresh, shown, until_line  # noqa: E402

TGT, SCHED = "e156_tgt", "e156_sched"
VIEW = "scheduler"


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
        tgt = await host_call(rig.port, "kernel", {
            "type": "create_agent", "parent_id": "user", "id": TGT, "handler_module": "ram_store.tools",
            "callable_by": {SCHED: ["reflect"]}})
        sch = await host_call(rig.port, "kernel", {
            "type": "create_agent", "parent_id": "user", "id": SCHED, "handler_module": "scheduler.tools",
            "can_call": {"$self": "*", TGT: ["reflect"]}, "callable_by": {"user": ["schedule", "unschedule", "tick_now", "list", "history", "watch"]}})
        info["made"] = [tgt.get("error"), sch.get("error")]
        r["0a both agents are made"] = "error" not in tgt and "error" not in sch
        async with rig.page(1200, 800) as pg:
            placed = await host_call(rig.port, f"{guid}|view_root", {"type": "place", "backend_id": SCHED, "x": 40, "y": 40, "width": 640, "height": 360})
            r["0b it is placed; the view shows no jobs"] = "placed" in placed and await until_line(pg, VIEW, lambda t: t == "nothing scheduled", 30)
            made = await host_call(rig.port, SCHED, {"type": "schedule", "target": TGT, "payload": {"type": "reflect"}, "interval_seconds": 3600})
            sid = made.get("schedule_id") or ""
            info["schedule"] = made
            r["1a schedule answers a schedule_id"] = bool(sid)
            await refresh(pg, VIEW)
            r["1b the view lists the job"] = await until_line(pg, VIEW, lambda t: t.startswith(f"{sid}  {TGT} · reflect  every 3600s") and "runs 0" in t, 20)
            r["1c and no run yet"] = await until_line(pg, VIEW, lambda t: t == "last: no runs yet", 5)
            tick = await host_call(rig.port, SCHED, {"type": "tick_now", "schedule_id": sid})
            info["tick"] = tick
            r["2 tick_now fires it with no error"] = tick.get("fired") is True and not tick.get("error")
            r["3a the view shows the run with no click"] = await until_line(pg, VIEW, lambda t: t.startswith(f"{sid}  ") and "runs 1" in t, 30)
            r["3b and its last result"] = await until_line(pg, VIEW, lambda t: t.startswith("last: ok"), 10)
            gone = await host_call(rig.port, SCHED, {"type": "unschedule", "schedule_id": sid})
            r["4a unschedule answers without error"] = "error" not in gone
            await refresh(pg, VIEW)
            r["4b the view shows nothing scheduled"] = await until_line(pg, VIEW, lambda t: t == "nothing scheduled", 20)
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
