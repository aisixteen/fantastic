"""E158 sidebar_render — the sidebar view as drawn in the canvas's left column (#608).

The column's iframe is null-origin: its document is read through CDP (`frame_eval`)."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402
from _lib.dev_surface import frame_eval  # noqa: E402

COLUMN = ".sidebar-col .sb-body iframe"


async def side(pg, expr: str):
    """`expr` (JS; the view's own document) evaluated in the sidebar view's frame; None while it has not drawn."""
    wrapped = f"(()=>{{if(!document.getElementById('tree')) return null; return {{v:({expr})}};}})()"
    hits = [h for h in await frame_eval(pg, wrapped) if isinstance(h, dict)]
    return hits[0]["v"] if hits else None


async def until(pg, expr: str, secs: float = 20) -> bool:
    for _ in range(int(secs * 5)):
        if await side(pg, expr):
            return True
        await asyncio.sleep(0.2)
    return False


async def click(pg, sel: str) -> bool:
    """A real mouse click at the middle of `sel` (an element of the view), through the column's iframe."""
    box = await side(pg, f"(()=>{{const e=document.querySelector({json.dumps(sel)}); if(!e) return null; const r=e.getBoundingClientRect(); return [r.x+r.width/2, r.y+r.height/2];}})()")
    frame = await pg.rect(COLUMN)
    if not box or not frame:
        return False
    x, y = frame["x"] + box[0], frame["y"] + box[1]
    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=x, y=y)
    await pg.mouse("mousePressed", x, y)
    await pg.mouse("mouseReleased", x, y)
    return True


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    (rig.proj / "docs").mkdir()
    (rig.proj / "docs" / "a.md").write_text("# a\n")
    (rig.proj / "notes.txt").write_text("notes\n")
    rig.init()
    r: dict = {}
    info: dict = {}

    async def run() -> None:
        async with rig.page(1400, 900) as pg:
            r["1a the column is drawn and the view has drawn in it"] = bool(await pg.until(f"!!document.querySelector({json.dumps(COLUMN)})", 30)) and await until(pg, "!!document.querySelector('.rail .k')", 40)
            parts = await side(pg, "({"
                "strip: !!document.querySelector('nav[aria-label=Kernels] .k.here'),"
                "head: !!document.querySelector('.head h1') && !!document.querySelector('button[aria-label=\"Hide the tree\"]'),"
                "agents: !!document.querySelector('nav[aria-label=Agents] .title b'),"
                "filter: !!document.querySelector('input[aria-label=\"Filter agents and files\"]'),"
                "files_switch: document.querySelector('button[role=switch][aria-label=\"Show files in the tree\"]')?.getAttribute('aria-checked'),"
                "waiting_hidden: document.getElementById('wait').hidden === true})")
            info["parts"] = parts
            r["1b kernel strip, header with Hide the tree, Agents nav, filter, Files switch (off)"] = bool(parts) and parts["strip"] and parts["head"] and parts["agents"] and parts["filter"] and parts["files_switch"] == "false"
            r["1c the Waiting for you box is hidden while nothing asks"] = bool(parts) and parts["waiting_hidden"]

            rows = await side(pg, "[...document.querySelectorAll('#tree [role=treeitem]')].map(e => ({id: e.dataset.id || null, path: e.dataset.path || null, pad: e.style.paddingLeft}))")
            info["rows"] = rows
            ids = [x["id"] for x in rows or [] if x["id"]]
            r["2a agent rows are drawn: the root first (depth 0), then user and files"] = bool(rows) and rows[0]["pad"] == "0px" and "user" in ids and "files" in ids
            r["2b no file row while the switch is off"] = bool(rows) and not any(x["path"] for x in rows)

            r["3a the Files switch on"] = await click(pg, "button[role=switch]")
            r["3b the project's files are rows inside the tree"] = await until(pg, "!!document.querySelector('#tree [data-path=\"docs\"]') && !!document.querySelector('#tree [data-path=\"notes.txt\"]')", 30)
            below = await side(pg, "(()=>{const f=document.querySelector('#tree [data-id=files]'), d=document.querySelector('#tree [data-path=\"docs\"]'); return !!f && !!d && parseFloat(d.style.paddingLeft) === parseFloat(f.style.paddingLeft) + 14 && !!(f.compareDocumentPosition(d) & Node.DOCUMENT_POSITION_FOLLOWING)})()")
            r["3c they sit one level under the files agent"] = bool(below)
            r["3d a folder opens in place"] = await click(pg, "#tree [data-path=\"docs\"] .tw") and await until(pg, "!!document.querySelector('#tree [data-path=\"docs/a.md\"]')", 20)

            before = await pg.js("document.querySelectorAll('.agent-frame').length")
            r["4a a click on the user agent row"] = await click(pg, "#tree [data-id=user] .main")
            r["4b opens its view: a cell is drawn"] = bool(await pg.until(f"document.querySelectorAll('.agent-frame').length > {before or 0}", 20))
            r["4c and the row is selected"] = await until(pg, "document.querySelector('#tree [data-id=user]')?.classList.contains('sel')", 10)

            await pg.open(rig.url)
            r["5a after a reload the view draws again"] = await until(pg, "!!document.querySelector('.rail .k')", 40)
            r["5b with the Files switch still on"] = await until(pg, "document.querySelector('button[role=switch]')?.getAttribute('aria-checked') === 'true'", 20)
            r["5c and the folder still open"] = await until(pg, "!!document.querySelector('#tree [data-path=\"docs/a.md\"]')", 30)

            r["6a the filter narrows the tree"] = await side(pg, "(()=>{const i=document.getElementById('filter'); i.value='a.md'; i.dispatchEvent(new Event('input')); return true})()") is True
            r["6b to the match and the rows above it"] = await until(pg, "(()=>{const t=[...document.querySelectorAll('#tree [role=treeitem]')]; return t.length >= 2 && t.length < 12 && !!document.querySelector('#tree [data-path=\"docs/a.md\"]') && !document.querySelector('#tree [data-path=\"notes.txt\"]')})()", 10)

            r["7a Hide the tree"] = await click(pg, "button[aria-label=\"Hide the tree\"]")
            r["7b collapses the column"] = bool(await pg.until("document.querySelector('.sidebar-col')?.classList.contains('collapsed')", 10))
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["8 no page error"] = not info["page_errors"]

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
