"""E84 home_start_stop — start, open, stop and Open Folder from the Home.

1. beta is registered and stopped: its row's Start reaches `running` within
   30 s, and the registry names its pid and canvas;
2. a click on its row opens its page: the page's `iframe.canvas-frame` shows the
   row's canvas url, and that frame loads (it is in `Page.getFrameTree`);
3. the page's Stop: its state card shows `stopping`, then `stopped`, within 20 s,
   and the registry's pid and canvas are null;
4. a second tab under a native shell's flag (`__FANTASTIC_SHELL__`, defined
   before load) has Open Folder: it goes through the flag's `pickFolder`, and the
   folder it names is added and reaches `running` within 30 s."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.home_live import HomeRig, canonical, click, os_shell, page_errors, q, verdict  # noqa: E402

# every status a page's state card shows, as it changes (the stopping beat can be short)
WATCH_CARD = """(()=>{window.__cards=[]; const seen=()=>{const c=document.querySelector('#home .page[data-id="%s"] .state-card');
  const s=c?c.dataset.status:'none'; if(window.__cards[window.__cards.length-1]!==s) window.__cards.push(s);};
  new MutationObserver(seen).observe(document.getElementById('home'),{subtree:true,childList:true,attributes:true}); seen(); return true;})()"""


def row_of(state: dict, path: str) -> dict:
    return next((x for x in state.get("rows", []) if x.get("path") == path), {})


def main() -> bool:
    rig = HomeRig(__file__)
    rig.build()
    r: dict = {}
    info: dict = {}

    async def drive() -> None:
        rig.start_home()
        beta = await rig.fantastic("beta", running=False)
        bp = canonical(beta)
        state = await rig.until_state("beta stopped", lambda s: row_of(s, bp).get("status") == "stopped", 30)
        bid = row_of(state, bp)["id"]
        row = f'#home .home-row[data-id="{bid}"]'
        async with rig.page() as pg:
            # 1. start
            r["start_clicked"] = await click(pg, f"{row} button[data-intent=start]")
            r["running"] = bool(await pg.until(f"{q(row + ' .status')}?.dataset.status === 'running'", 30))
            reg = rig.registry(bid) or {}
            info["registry_running"] = reg
            r["registry_pid_canvas"] = isinstance(reg.get("pid"), int) and bool(reg.get("canvas"))
            # 2. open its page
            r["row_clicked"] = await click(pg, f"{row} .name")
            state = await rig.until_state("beta's page shown", lambda s: s["selection"] == bid, 10)
            url = (row_of(state, bp).get("canvas") or {}).get("url")
            frame = f'#home .page[data-id="{bid}"] iframe.canvas-frame'
            r["frame_src"] = bool(await pg.until(f"{q(frame)}?.src === {json.dumps(url)}", 10))
            loaded = False
            for _ in range(100):
                tree = await pg.cdp("Page.getFrameTree")
                kids = [c["frame"].get("url", "") for c in tree["frameTree"].get("childFrames", [])]
                if any(u.startswith(url or "\0") for u in kids):
                    loaded = True
                    break
                await asyncio.sleep(0.2)
            info["child_frames"] = kids
            r["frame_loaded"] = loaded
            # 3. stop from the page
            await pg.js(WATCH_CARD % bid)
            r["stop_clicked"] = await click(pg, f'#home .page[data-id="{bid}"] .controls button[data-intent=stop]')
            card = q(f'#home .page[data-id="{bid}"] .state-card')
            r["stopped"] = bool(await pg.until(f"{card}?.dataset.status === 'stopped'", 20))
            cards = await pg.js("window.__cards") or []
            info["cards"] = cards
            r["stopping_then_stopped"] = "stopping" in cards and cards[-1] == "stopped" and cards.index("stopping") < len(cards) - 1
            cleared = False
            for _ in range(100):
                reg = rig.registry(bid) or {}
                if reg and reg.get("pid") is None and reg.get("canvas") is None:
                    cleared = True
                    break
                await asyncio.sleep(0.2)
            info["registry_stopped"] = reg
            r["registry_cleared"] = cleared
            r["no_page_errors"] = not page_errors(pg)
            info["page_errors"] = page_errors(pg)[:5]
        # 4. Open Folder through a native shell's pickFolder
        delta = rig.art / "delta"
        delta.mkdir()
        rig.folders["delta"] = delta
        os_name, webview = os_shell()
        flag = (f"window.__FANTASTIC_SHELL__ = Object.freeze({{name: 'probe', os: {json.dumps(os_name)}, webview: {json.dumps(webview)},"
                f" backdrop: 'web', pickFolder: async () => [{json.dumps(str(delta))}]}});")
        async with rig.page(before_load=flag) as pg:
            r["open_folder_shown"] = bool(await pg.until(f"!!{q('#home button.open-folder')} && !{q('#home form.open-path')}", 10))
            r["open_hint"] = bool(await pg.js("document.body.innerText.includes('or drop a folder anywhere')"))
            r["open_folder_clicked"] = await click(pg, "#home button.open-folder")
            dp = canonical(delta)
            st = await rig.until_state("delta running", lambda s: row_of(s, dp).get("status") == "running", 30)
            did = row_of(st, dp)["id"]
            status = q(f'#home .home-row[data-id="{did}"] .status')
            r["delta_row_running"] = bool(await pg.until(f"{status}?.dataset.status === 'running'", 10))
            r["no_page_errors_shell"] = not page_errors(pg)
            info["page_errors_shell"] = page_errors(pg)[:5]

    try:
        asyncio.run(drive())
    finally:
        rig.stop_all()
    ok = bool(r) and all(r.values())
    rig.summary["checks"], rig.summary["info"] = r, info
    note = f"start → running, a live frame, stop → {' → '.join(info.get('cards', []))}, Open Folder through pickFolder" if ok else \
        f"failed: {', '.join(k for k, v in r.items() if not v)} · {json.dumps(info)[:1500]}"
    return verdict(*rig.finish(ok, note))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
