"""E87 skins — the one skin per backdrop and the read-only skin agent (phase 4, #49): see README."""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, html_cell, spot, verdict  # noqa: E402
from helpers import oslayer  # noqa: E402

TERM_VERBS = ["watch", "status", "start", "write", "ack", "resize", "output", "paste_image"]
CELL, BACKEND = "cell_skin0000", "exec_bridge_skin0000"

FRAME = "document.querySelector('.agent-frame[data-id=\"a\"]')"
BEZEL = f"getComputedStyle({FRAME}, '::before').backgroundColor"
BODY = "getComputedStyle(document.body).backgroundColor"
ACCENT = "getComputedStyle(document.documentElement).getPropertyValue('--fc-color-accent-base').trim()"
SKIN = "document.documentElement.dataset.skin"
BACKDROP = "document.documentElement.dataset.backdrop"
DRAWN = "document.getElementById('bg')?.dataset.background"
TERM_FG = f"document.querySelector('.agent-frame[data-id=\"{CELL}\"] .frame-body')?.xterm?.options?.theme?.foreground"
SHELL = 'window.__FANTASTIC_SHELL__ = Object.freeze({name: "probe", os: "macos", webview: "wkwebview", backdrop: "native"});'

def terminal_pair() -> tuple[dict, tuple[str, dict]]:
    """One terminal cell and its exec agent, as E65 `seeds(1)` makes them."""
    cell = {"id": CELL, "handler_module": "terminal_view.ts", "backend_id": BACKEND, "owns_backend": False,
            "can_call": {"$self": "*", BACKEND: TERM_VERBS}, "callable_by": {"$self": "*"},
            "x": 520, "y": 120, "width": 360, "height": 220}
    host = (FOLDER, {"id": BACKEND, "handler_module": "exec_bridge.tools", "cwd": ".", "inherit_env": ["*"], "shell": "login",
                     "ingress_rule": "allow_all_ingress", "cell_id": CELL, "can_call": {"$self": "*"},
                     "callable_by": {"$self": "*", "mcp": ["*"], "user": ["*"], "canvas_root": ["*"], "canvas_ws": ["watch"]}})
    return cell, host


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    r: dict = {}
    info: dict = {}
    web = rig.proj / ".fantastic" / "web" / "agents" / "canvas" / "agents"
    skin_yaml = web / "skin" / "agent.yaml"
    bg_yaml = web / "view_root" / "agents" / "background" / "agent.yaml"

    def on_disk(p: Path) -> dict:
        # the page's loader replaces the record atomically: on Windows an open
        # that meets the rename is a sharing violation, which oslayer.read_text
        # opens again (a missing record is simply not there yet)
        try:
            return yaml.safe_load(oslayer.read_text(p)) or {}
        except FileNotFoundError:
            return {}

    async def disk_until(p: Path, ok, timeout: float = 10.0) -> dict:
        """The record once `ok(record)` holds — the loader's write landed — or
        the last one read when it never does."""
        deadline = time.time() + timeout
        d = on_disk(p)
        while not ok(d) and time.time() < deadline:
            await asyncio.sleep(0.1)
            d = on_disk(p)
        return d

    try:
        cell, host = terminal_pair()
        rig.init(cells=[html_cell("a", "A", 120, 120, "<p>A</p>", 320, 200), cell], host_agents=[host])

        async def run() -> None:
            from canvas import host_call

            call = lambda payload: host_call(rig.port, f"{rig.page_guid()}|skin", payload)  # noqa: E731
            async with rig.page(1400, 900) as pg:
                await pg.until("document.querySelectorAll('.agent-frame').length === 2", 20)
                await pg.until(f"!!{TERM_FG}", 20)
                # 1-2: day, the web default
                info["1_skin"], info["1_backdrop"] = await pg.js(SKIN), await pg.js(BACKDROP)
                info["2_body"], info["2_bezel"], info["2_accent"] = await pg.js(BODY), await pg.js(BEZEL), await pg.js(ACCENT)
                await pg.until(f"{DRAWN} === 'drawn'", 10)
                info["2_drawn"] = await pg.js(DRAWN)
                info["2_term_fg"] = await pg.js(TERM_FG)
                # 3: the skin agent is read only: get and schema answer; list, use, add, delete are unknown verbs
                info["3_get"] = await call({"type": "get", "name": "day"})
                info["3_schema"] = await call({"type": "schema"})
                info["3_refused"] = {v: (await call({"type": v, "name": "glass"})).get("reason") for v in ("list", "use", "add", "delete")}
                await asyncio.sleep(0.5)
                info["3_unchanged"] = await pg.js(f"{SKIN} === 'day' && {BEZEL} === 'rgba(255, 255, 255, 0.58)'")
                # 4: the record holds no skin choice
                info["4_record"] = await disk_until(skin_yaml, lambda d: d.get("handler_module") == "skin.ts")
                # 5: the backgrounds panel has no Skins or Themes section
                await spot(pg, 900, 600, "backgrounds")
                await pg.until("[...document.querySelectorAll('.bgs h2')].length >= 1", 15)
                info["5_headings"] = await pg.js("[...document.querySelectorAll('.bgs h2')].map(h=>h.firstChild.textContent)")
                info["5_skin_rows"] = await pg.js("document.querySelectorAll('.bgs [data-skin]').length")
                info["console_plain"] = [x for x in pg.logs if "WebGL" not in x][:6]

                # ── 6: the same canvas, hosted by an (emulated) native shell ──
                await pg.cdp("Page.addScriptToEvaluateOnNewDocument", source=SHELL)
                await pg.open(rig.url)
                await pg.until("!!document.getElementById('canvas')", 30)
                await pg.until("document.querySelectorAll('.agent-frame').length >= 2", 20)
                await pg.until(f"!!{TERM_FG}", 20)
                await pg.until(f"{DRAWN} === 'none'", 10)
                info["6_skin"], info["6_backdrop"] = await pg.js(SKIN), await pg.js(BACKDROP)
                info["6_body"] = await pg.js(BODY)
                info["6_bezel"] = await pg.js(BEZEL)
                info["6_drawn"] = await pg.js(DRAWN)
                info["6_bg_record"] = len(str(on_disk(bg_yaml).get("bg_source") or ""))
                info["6_term_fg"] = await pg.js(TERM_FG)
                info["console_native"] = [x for x in pg.logs if "WebGL" not in x][:6]

        asyncio.run(run())

        def alpha(c: object) -> float:
            s = str(c)
            return float(s.rsplit(",", 1)[1].strip(" )")) if s.startswith("rgba(") else 1.0

        r["1 a plain tab is day on the web backdrop"] = info.get("1_skin") == "day" and info.get("1_backdrop") == "web"
        r["2 day's computed styles are the day tokens'"] = (info.get("2_body") == "rgb(237, 237, 237)" and info.get("2_bezel") == "rgba(255, 255, 255, 0.58)"
                                                      and info.get("2_accent") == "rgb(17 17 17)" and info.get("2_drawn") == "drawn"
                                                      and info.get("2_term_fg") == "#0b0b0b")
        r["3 get and schema answer; list, use, add and delete are unknown verbs; nothing changes"] = (
            (info.get("3_get") or {}).get("backdrop") == "web" and bool((info.get("3_schema") or {}).get("tokens"))
            and set((info.get("3_refused") or {}).values()) == {"unhandled"} and len(info.get("3_refused") or {}) == 4 and info.get("3_unchanged") is True)
        r["4 the skin record holds no skin choice"] = "skins" not in (info.get("4_record") or {}) and "user_skins" not in (info.get("4_record") or {})
        r["5 the backgrounds panel has no Skins or Themes section"] = (info.get("5_headings") or []) == ["Backgrounds"] and info.get("5_skin_rows") == 0
        r["6 native: glass, a transparent page, translucent bezels, no background drawn, its record kept, the terminal's glass fg"] = (
            info.get("6_skin") == "glass" and info.get("6_backdrop") == "native" and info.get("6_body") == "rgba(0, 0, 0, 0)"
            and alpha(info.get("6_bezel")) < 0.6 and info.get("6_drawn") == "none" and (info.get("6_bg_record") or 0) > 0
            and info.get("6_term_fg") == "#ececec")
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:3000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
