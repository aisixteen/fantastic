"""E68 html_bundle — an html cell created, edited and bundled the way a coding
agent does it (owner 2026-10-09: "all same with html agent — creation, edit,
bundling"):

a) double-click → `html` places an html cell with its starter body;
b) `set_html` sent to `<guid>|<cell>` through the host re-renders it;
c) its body written as a page folder bundle (`.fantastic/web/bundles/card_e68/`,
   `base: html_agent`) is a tile in the popup's `page · bundles` group;
d) that tile places an instance showing the body;
e) the bundle's html rewritten reaches the placed instance (not the edited cell,
   whose body is its own)."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, spot, verdict  # noqa: E402

NAME = "card_e68"
BODY_A = "<div style='font:20px system-ui;color:#cde;padding:14px'>HTML-E68-A</div>"
BODY_B = "<div style='font:20px system-ui;color:#fc8;padding:14px'>HTML-E68-B</div>"


def bundle_yaml(body: str) -> str:
    return f"base: html_agent\nsentence: A card (E68).\nhtml: |\n  {body}\n"


def has(cell: str, text: str) -> str:
    """JS: the cell's rendered body contains `text` (an iframe's srcdoc or the cell's own DOM)."""
    q = f".agent-frame[data-id={json.dumps(cell)}]"
    return f"(()=>{{const f=document.querySelector({json.dumps(q)}); if(!f) return false; const i=f.querySelector('iframe'); return ((i?.srcdoc||'')+(i?.contentDocument?.body?.innerHTML||'')+f.innerHTML).includes({json.dumps(text)})}})()"


CELLS = "[...document.querySelectorAll('.agent-frame')].map(f=>f.dataset.id)"


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    try:
        rig.init()

        async def run() -> None:
            sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "e2e" / "runners"))
            from canvas import host_call

            async with rig.page(1500, 950) as pg:
                # a) created from the popup
                await spot(pg, 300, 260, "html")
                ok = await pg.until(f"{CELLS}.some(id=>id.startsWith('html_'))", 20)
                cell = await pg.js(f"{CELLS}.find(id=>id.startsWith('html_'))") or ""
                info["cell"] = cell
                r["created"] = bool(ok) and bool(await pg.until(has(cell, "an empty html panel"), 10))
                # b) edited through the host, as a coding agent's send
                guid = rig.page_guid()
                info["set_html"] = await host_call(rig.port, f"{guid}|{cell}", {"type": "set_html", "bytes": BODY_A})
                r["edited"] = bool(await pg.until(has(cell, "HTML-E68-A"), 15))
                # c) bundled: the body as a page folder → a popup tile
                got = await host_call(rig.port, f"{guid}|{cell}", {"type": "get_html"})
                body = got.get("html", "") if isinstance(got, dict) else ""
                folder = rig.proj / ".fantastic" / "web" / "bundles" / NAME
                folder.mkdir(parents=True)
                (folder / "bundle.yaml").write_text(bundle_yaml(body))
                await asyncio.sleep(2.0)
                for c in (1, 2):
                    await pg.mouse("mousePressed", 900, 200, c)
                    await pg.mouse("mouseReleased", 900, 200, c)
                await pg.until("!!document.querySelector('.action-popup .tile')", 10)
                tiles = await pg.js("[...document.querySelectorAll('.action-popup .tile[data-section=page_bundles] .lb')].map(e=>e.textContent)")
                info["page_bundle_tiles"] = tiles
                r["bundle_is_a_tile"] = NAME in (tiles or [])
                await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="Escape", code="Escape", windowsVirtualKeyCode=27)
                await asyncio.sleep(0.4)
                # d) placed from the popup, showing the body
                await spot(pg, 900, 200, NAME)
                await pg.until(f"{CELLS}.some(id=>id.startsWith({json.dumps(NAME + '_')}))", 15)
                inst = await pg.js(f"{CELLS}.find(id=>id.startsWith({json.dumps(NAME + '_')}))") or ""
                info["instance"] = inst
                r["instance_placed"] = inst != "" and bool(await pg.until(has(inst, "HTML-E68-A"), 15))
                # e) the bundle edited → its instance draws it; the edited cell keeps its own
                (folder / "bundle.yaml").write_text(bundle_yaml(BODY_B))
                r["bundle_edit_reaches_instance"] = inst != "" and bool(await pg.until(has(inst, "HTML-E68-B"), 15))
                r["own_body_kept"] = bool(await pg.js(has(cell, "HTML-E68-A")))
                info["page_errors"] = pg.logs[:5]

        asyncio.run(run())
    finally:
        rig.stop()
    print(json.dumps(info, indent=1))
    ok = bool(r) and all(r.values())
    rig.summary["checks"] = r
    rig.summary["info"] = info
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(ok, "all checks pass" if ok else f"failed: {', '.join(failed)}"))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
