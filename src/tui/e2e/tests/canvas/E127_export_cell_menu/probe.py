"""E127 export_cell_menu — the cell menu's "Export…" writes a blueprint folder (#268).

A real host and a real page. The seeded team (`lead` > `mid` > `leaf`) is shown by
one cell (`cell_e127`, `backend_id: lead`); a second cell (`cell_e127_page`) has
no host agent. The page's `prompt` is replaced (a native dialog is not CDP
friendly) and answers `out/menu`.

a) a right-click (a CDP mouse event, button right) on the cell's head opens the
   menu: an "Export…" item is among its buttons;
b) a click on it asks the host `user export_subtree` (the host's own stream shows
   the call), the page toasts "exported 3 agents to out/menu", and `out/menu`
   holds the blueprint: nested, ports, no key / runtime field / row text;
c) the page-only cell's menu "Export…" toasts "has no host agent", asks the host
   nothing more and writes no folder;
d) a prompt answered with nothing writes nothing; no page error is logged."""

from __future__ import annotations

import asyncio
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, html_cell, verdict  # noqa: E402
from _lib.export_live import check_folder, team_agents  # noqa: E402

CELL, PAGE_ONLY = "cell_e127", "cell_e127_page"


def frame(cid: str) -> str:
    return f"document.querySelector('.agent-frame[data-id=\"{cid}\"]')"


def calls(rig: Rig) -> int:
    """How many times the host's stream shows `user call export_subtree`."""
    return sum(1 for ln in rig.log.read_text().splitlines() if re.match(r"^\[\d+\] user call export_subtree\b", ln))


async def right_click(pg, cid: str) -> None:
    r = await pg.rect(f".agent-frame[data-id=\"{cid}\"] .frame-head")
    x, y = r["x"] + r["w"] / 2, r["y"] + r["h"] / 2
    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=x, y=y)
    await pg.cdp("Input.dispatchMouseEvent", type="mousePressed", x=x, y=y, button="right", clickCount=1)
    await pg.cdp("Input.dispatchMouseEvent", type="mouseReleased", x=x, y=y, button="right", clickCount=1)


async def click_item(pg, sel: str) -> None:
    """A real mouse click (CDP) on the middle of `sel`."""
    b = await pg.rect(sel)
    if b is None:
        return  # nothing to click: the checks around it say so
    x, y = b["x"] + b["w"] / 2, b["y"] + b["h"] / 2
    await pg.cdp("Input.dispatchMouseEvent", type="mouseMoved", x=x, y=y)
    await pg.mouse("mousePressed", x, y)
    await pg.mouse("mouseReleased", x, y)


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.proj.mkdir(parents=True, exist_ok=True)
    (rig.proj / "out").mkdir()
    r: dict = {}
    info: dict = {}
    cells = [
        html_cell(CELL, "the team", 60, 60, "<p>the team</p>", backend_id="lead"),
        html_cell(PAGE_ONLY, "page only", 460, 60, "<p>no host agent</p>"),
    ]
    rig.init(cells=cells, host_agents=team_agents())

    async def run() -> None:
        toasts = "[...document.querySelectorAll('.canvas-toast')].map(t=>t.textContent).join(' | ')"
        async with rig.page(1400, 900) as pg:
            r["a0 both cells are on screen"] = bool(await pg.until(f"!!{frame(CELL)} && !!{frame(PAGE_ONLY)}", 20))
            # a native dialog is not CDP friendly: the page's prompt answers for the human
            await pg.js("window.__answer = 'out/menu'; window.prompt = () => window.__answer; true")
            await right_click(pg, CELL)
            item = ".cell-menu button[data-intent=\"export\"]"
            r["a1 the right-click opens the menu with Export…"] = bool(await pg.until(f"document.querySelector('{item}')?.textContent === 'Export…'", 10))
            before = calls(rig)
            await click_item(pg, item)
            r["b1 the page toasts the export"] = bool(await pg.until(f"({toasts}).includes('exported 3 agents to out/menu')", 20))
            r["b2 the host's stream shows user call export_subtree"] = calls(rig) == before + 1
            for k, v in check_folder(rig.proj / "out" / "menu", rig.proj).items():
                r[f"b3 folder: {k}"] = v
            # c) a cell with no host agent
            await pg.js("document.querySelectorAll('.canvas-toast').forEach(t=>t.remove()); true")
            await right_click(pg, PAGE_ONLY)
            r["c1 its menu opens"] = bool(await pg.until(f"!!document.querySelector('{item}')", 10))
            await click_item(pg, item)
            r["c2 it toasts that it has no host agent"] = bool(await pg.until(f"({toasts}).includes('has no host agent')", 10))
            await asyncio.sleep(0.5)
            r["c3 the host was asked nothing more"] = calls(rig) == before + 1
            # d) an empty answer writes nothing
            await pg.js("window.__answer = ''; true")
            await right_click(pg, CELL)
            await pg.until(f"!!document.querySelector('{item}')", 10)
            await click_item(pg, item)
            await asyncio.sleep(0.8)
            r["d1 an empty answer asks the host nothing"] = calls(rig) == before + 1
            r["d2 and the project holds only out/menu"] = sorted(p.name for p in (rig.proj / "out").iterdir()) == ["menu"]
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["d3 no page errors"] = not info["page_errors"]

    try:
        asyncio.run(run())
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:3000])
    bad = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not bad, f"{len(r) - len(bad)}/{len(r)} pass" + ("" if not bad else f" — failed: {'; '.join(bad)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
