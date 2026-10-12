"""E77 update_reloads — the view's `update` with a new body re-renders the cell at once."""

from __future__ import annotations

import asyncio
import json
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, html_cell, verdict  # noqa: E402

SRC = "(id)=>document.querySelector('.agent-frame[data-id='+JSON.stringify(id)+'] iframe')?.srcdoc||''"


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        cells = [
            html_cell("t1", "Trusted", 100, 100, "<p id=m>MARK-v1</p>", can_call={"$self": "*"}),
            {**html_cell("s1", "Sandboxed", 500, 100, "<p id=m>MARK-v1</p>", can_call={"$self": "*"}), "trusted": False},
        ]
        rig.init(cells=cells)

        def send(target: str, payload: dict) -> dict:
            p = subprocess.run([str(rig.binary), "send", json.dumps({"target": target, "payload": payload})],
                               cwd=rig.proj, env=rig.env(), capture_output=True, text=True, timeout=60)
            try:
                return json.loads(p.stdout).get("result") or {}
            except ValueError:
                return {"error": (p.stdout + p.stderr)[-300:]}

        async def run() -> None:
            async with rig.page(1400, 900) as pg:
                await pg.until(f"({SRC})('t1').includes('MARK-v1') && ({SRC})('s1').includes('MARK-v1')", 20)
                # no reload from here on: the page's own navigation count must stay 1
                await pg.js("window.__loads = (window.__loads||0) + 1")
                # the page's kernel id, once the host knows it (the page connects after its frames draw)
                guid = ""
                for _ in range(100):
                    guid = rig.page_guid()
                    if guid:
                        break
                    await asyncio.sleep(0.1)
                for cid in ("t1", "s1"):
                    info[f"{cid}_reply"] = send(f"{guid}|view_root", {"type": "update", "id": cid, "bytes": "<p id=m>MARK-v2</p>",
                                                                      "fields": {"can_call": {"$self": "*", "static_files": ["list"]}}})
                for cid in ("t1", "s1"):
                    r[f"{cid}: shows the new body at once"] = bool(await pg.until(f"({SRC})({json.dumps(cid)}).includes('MARK-v2')", 5))
                r["no page reload happened"] = await pg.js("window.__loads === 1")
                import time as _t
                t0 = _t.time()
                while _t.time() - t0 < 10 and not all("MARK-v2" in (c.get("html") or "") for c in rig.cells() if c.get("id") in ("t1", "s1")):
                    await asyncio.sleep(0.1)
                info["saved_after_s"] = round(_t.time() - t0, 2)
                info["records"] = {c.get("id"): c.get("html") for c in rig.cells() if c.get("id") in ("t1", "s1")}
                r["the record holds the new body"] = all("MARK-v2" in (info["records"].get(c) or "") for c in ("t1", "s1"))
                await pg.open(rig.url)
                r["after a reload: still the new body"] = bool(await pg.until(f"({SRC})('t1').includes('MARK-v2') && ({SRC})('s1').includes('MARK-v2')", 20))

        asyncio.run(run())
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:1500])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
