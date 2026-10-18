"""E87 skins — design tokens and the skin agent (phase 4, #49): see README."""

from __future__ import annotations

import asyncio
import json
import sys
import time
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import FOLDER, Rig, html_cell, spot, verdict  # noqa: E402

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

RED = {"color": {
    "accent": {"base": {"$value": {"colorSpace": "srgb", "components": [1, 0.2, 0.2], "hex": "#ff3333"}}},
    "surface": {"bezel": {"$value": {"colorSpace": "srgb", "components": [0.2, 0, 0], "hex": "#330000", "alpha": 0.9}}},
}}
BAD = {"color": {"accent": {"base": {"$value": {"colorSpace": "srgb", "components": [2, 0, 0]}}}}}


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
        try:
            return yaml.safe_load(p.read_text()) or {}
        except (OSError, yaml.YAMLError):
            return {}

    try:
        cell, host = terminal_pair()
        rig.init(cells=[html_cell("a", "A", 120, 120, "<p>A</p>", 320, 200), cell], host_agents=[host])

        async def run() -> None:
            from canvas import host_call

            call = lambda payload: host_call(rig.port, f"{rig.page_guid()}|skin", payload)  # noqa: E731
            async with rig.page(1400, 900) as pg:
                await pg.until("document.querySelectorAll('.agent-frame').length === 2", 20)
                await pg.until(f"!!{TERM_FG}", 20)
                # 1-2: dark, today's look
                info["1_skin"], info["1_backdrop"] = await pg.js(SKIN), await pg.js(BACKDROP)
                info["2_body"], info["2_bezel"], info["2_accent"] = await pg.js(BODY), await pg.js(BEZEL), await pg.js(ACCENT)
                await pg.until(f"{DRAWN} === 'drawn'", 10)
                info["2_drawn"] = await pg.js(DRAWN)
                info["2_term_fg"] = await pg.js(TERM_FG)
                # 3-4: an AI's add, and a refused one
                await pg.js("window.__skinProbe = 1")
                info["3_add"] = await call({"type": "add", "name": "probe-red", "base": "dark", "tokens": RED, "note": "the probe's"})
                info["4_bad"] = await call({"type": "add", "name": "probe-bad", "base": "dark", "tokens": BAD})
                # 5: use it — live, no reload
                info["5_use"] = await call({"type": "use", "name": "probe-red"})
                t0 = time.time()
                info["5_live"] = bool(await pg.until(f"{BEZEL} === 'rgba(51, 0, 0, 0.9)' && {ACCENT} === 'rgb(255 51 51)'", 3))
                info["5_seconds"] = round(time.time() - t0, 2)
                info["5_no_reload"] = await pg.js("window.__skinProbe === 1")
                info["5_skin"] = await pg.js(SKIN)
                # 6: a native skin is refused in a browser tab
                info["6_glass"] = await call({"type": "use", "name": "glass"})
                await asyncio.sleep(0.5)
                info["6_unchanged"] = await pg.js(f"{SKIN} === 'probe-red' && {BEZEL} === 'rgba(51, 0, 0, 0.9)'")
                # 7: back to dark
                info["7_use"] = await call({"type": "use", "name": "dark"})
                info["7_dark"] = bool(await pg.until(f"{BEZEL} === 'rgba(15, 15, 25, 0.88)' && {ACCENT} === 'rgb(183 140 255)' && {SKIN} === 'dark'", 3))
                # 8: on disk
                for _ in range(40):
                    d = on_disk(skin_yaml)
                    if (d.get("skins") or {}).get("web") == "dark" and any(u.get("name") == "probe-red" for u in d.get("user_skins") or []):
                        break
                    await asyncio.sleep(0.25)
                d = on_disk(skin_yaml)
                info["8_disk"] = {"skins": d.get("skins"), "user_skins": [u.get("name") for u in d.get("user_skins") or []]}
                # 9: the panel's Skins section
                await spot(pg, 900, 600, "canvas theme")
                await pg.until("[...document.querySelectorAll('.bgs h2')].length >= 3", 15)
                info["9_headings"] = await pg.js("[...document.querySelectorAll('.bgs h2')].map(h=>h.firstChild.textContent)")
                info["9_rows"] = await pg.js("[...document.querySelectorAll('.bgs [data-skin]')].map(b=>b.dataset.skin)")
                await pg.js("document.querySelector('.bgs [data-skin=\"probe-red\"]')?.click()")
                info["9_click"] = bool(await pg.until(f"{SKIN} === 'probe-red' && {BEZEL} === 'rgba(51, 0, 0, 0.9)'", 5))
                await call({"type": "use", "name": "dark"})
                await pg.until(f"{SKIN} === 'dark'", 5)
                info["console_plain"] = [x for x in pg.logs if "WebGL" not in x][:6]

                # ── 10-12: the same canvas, hosted by an (emulated) native shell ──
                await pg.cdp("Page.addScriptToEvaluateOnNewDocument", source=SHELL)
                await pg.open(rig.url)
                await pg.until("!!document.getElementById('canvas')", 30)
                await pg.until("document.querySelectorAll('.agent-frame').length >= 2", 20)
                await pg.until(f"!!{TERM_FG}", 20)
                await pg.until(f"{DRAWN} === 'none'", 10)
                info["10_skin"], info["10_backdrop"] = await pg.js(SKIN), await pg.js(BACKDROP)
                info["10_body"] = await pg.js(BODY)
                info["10_bezel"] = await pg.js(BEZEL)
                info["10_drawn"] = await pg.js(DRAWN)
                info["10_bg_record"] = len(str(on_disk(bg_yaml).get("bg_source") or ""))
                info["10_term_fg"] = await pg.js(TERM_FG)
                await pg.js("window.__skinProbe = 2")
                info["11_use_dark"] = await call({"type": "use", "name": "dark", "backdrop": "native"})
                info["11_dark"] = bool(await pg.until(f"{BODY} === 'rgb(6, 6, 12)' && {DRAWN} === 'drawn' && {TERM_FG} === '#e5e5e5'", 5))
                info["11_use_glass"] = await call({"type": "use", "name": "glass", "backdrop": "native"})
                info["11_glass"] = bool(await pg.until(f"{BODY} === 'rgba(0, 0, 0, 0)' && {DRAWN} === 'none' && {TERM_FG} === '#ececec'", 5))
                info["11_no_reload"] = await pg.js("window.__skinProbe === 2")
                await asyncio.sleep(1.0)
                info["12_disk"] = on_disk(skin_yaml).get("skins")
                info["console_native"] = [x for x in pg.logs if "WebGL" not in x][:6]

        asyncio.run(run())

        def alpha(c: object) -> float:
            s = str(c)
            return float(s.rsplit(",", 1)[1].strip(" )")) if s.startswith("rgba(") else 1.0

        bad = info.get("4_bad") or {}
        r["1 a plain tab is dark on the web backdrop"] = info.get("1_skin") == "dark" and info.get("1_backdrop") == "web"
        r["2 dark's computed styles are today's"] = (info.get("2_body") == "rgb(6, 6, 12)" and info.get("2_bezel") == "rgba(15, 15, 25, 0.88)"
                                                      and info.get("2_accent") == "rgb(183 140 255)" and info.get("2_drawn") == "drawn"
                                                      and info.get("2_term_fg") == "#e5e5e5")
        r["3 add answers added"] = (info.get("3_add") or {}).get("added") == "probe-red"
        r["4 a bad colour is refused, naming color.accent.base"] = bad.get("reason") == "bad_arg" and "color.accent.base" in str(bad.get("error"))
        r["5 use applies it live, without a reload"] = info.get("5_live") is True and info.get("5_no_reload") is True and info.get("5_skin") == "probe-red"
        r["6 glass is refused in a browser tab; nothing changes"] = (info.get("6_glass") or {}).get("reason") == "bad_arg" and info.get("6_unchanged") is True
        r["7 use dark brings dark back"] = info.get("7_dark") is True
        r["8 the record on disk: skins.web dark, user skin probe-red"] = ((info.get("8_disk") or {}).get("skins") or {}).get("web") == "dark" and "probe-red" in ((info.get("8_disk") or {}).get("user_skins") or [])
        r["9 the panel's third section Skins lists them; a click applies"] = ((info.get("9_headings") or [])[2:3] == ["Skins"]
                                                                            and {"dark", "glass", "probe-red"} <= set(info.get("9_rows") or [])
                                                                            and info.get("9_click") is True)
        r["10 native: glass, a transparent page, translucent bezels, no background drawn, its record kept, the terminal's glass fg"] = (
            info.get("10_skin") == "glass" and info.get("10_backdrop") == "native" and info.get("10_body") == "rgba(0, 0, 0, 0)"
            and alpha(info.get("10_bezel")) < 0.6 and info.get("10_drawn") == "none" and (info.get("10_bg_record") or 0) > 0
            and info.get("10_term_fg") == "#ececec")
        r["11 the native slot switches live: dark draws, glass clears; no reload"] = info.get("11_dark") is True and info.get("11_glass") is True and info.get("11_no_reload") is True
        r["12 the web slot on disk is untouched"] = (info.get("12_disk") or {}).get("web") == "dark"
    finally:
        rig.stop()
    rig.summary["checks"] = r
    rig.summary["info"] = info
    print(json.dumps(info, indent=1, default=str)[:3000])
    failed = [k for k, v in r.items() if not v]
    return verdict(*rig.finish(not failed, f"{len(r) - len(failed)}/{len(r)} pass" + ("" if not failed else f" — failed: {'; '.join(failed)}")))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
