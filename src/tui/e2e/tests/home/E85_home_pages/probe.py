"""E85 home_pages — pages, the nav pill, the shortcuts, and a host restart.

alpha and beta run; both are opened by row clicks, so the pill holds two pages
and beta is shown. Once both canvas frames have loaded, their `data-born` is
recorded and every child-frame navigation from then on is counted:

- the pill to alpha, mod+2 to beta, mod+1 to alpha — after each, the home's own
  `list` names the shown page as its selection; mod+0 shows the Home;
- no child frame navigated, and both frames are the ones born before;
- a second tab shows the same `aria-current` page (the state is the agent's),
  and a shortcut sent there moves the first tab too;
- mod+W with alpha shown closes alpha's page; alpha still runs (its lock is
  held, its canvas answers);
- the host restarts with beta selected: the reopened Home shows beta's page
  current and its frame loads its canvas again; alpha's page stays closed and
  alpha still runs."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import http_get  # noqa: E402
from _lib.home_live import (  # noqa: E402
    HomeRig,
    StateLog,
    canonical,
    click,
    key,
    lock_pid,
    page_errors,
    q,
    verdict,
)

CURRENT = ("(()=>{const b=document.querySelector('#home nav.nav-pill button[aria-current=page]');"
           " return b ? (b.classList.contains('nav-home') ? 'home' : b.dataset.id) : null;})()")


def row_of(state: dict, path: str) -> dict:
    return next((x for x in state.get("rows", []) if x.get("path") == path), {})


async def frames_loaded(pg, urls: list[str], timeout: float = 20) -> bool:
    """Every url in `urls` is a child frame in the page's frame tree."""
    for _ in range(int(timeout * 5)):
        tree = await pg.cdp("Page.getFrameTree")
        kids = [c["frame"].get("url", "") for c in tree["frameTree"].get("childFrames", [])]
        if all(any(k.startswith(u) for k in kids) for u in urls):
            return True
        await asyncio.sleep(0.2)
    return False


def main() -> bool:
    rig = HomeRig(__file__)
    rig.build()
    r: dict = {}
    info: dict = {}

    async def drive() -> None:
        rig.start_home()
        alpha = await rig.fantastic("alpha")
        beta = await rig.fantastic("beta")
        ap, bp = canonical(alpha), canonical(beta)
        state = await rig.until_state("alpha and beta running",
                                      lambda s: row_of(s, ap).get("status") == "running" and row_of(s, bp).get("status") == "running", 30)
        aid, bid = row_of(state, ap)["id"], row_of(state, bp)["id"]
        urls = {aid: row_of(state, ap)["canvas"]["url"], bid: row_of(state, bp)["canvas"]["url"]}

        async def selection_is(want: str | None, what: str) -> bool:
            """The home agent's selection is `want`, and this tab shows it current."""
            shown = "home" if want is None else want
            tab = bool(await pg.until(f"{CURRENT} === {json.dumps(shown)}", 10))
            try:
                await rig.until_state(what, lambda s: s["selection"] == want, 10)
                agent = True
            except SystemExit:
                agent = False
            return tab and agent

        async with rig.page() as pg:
            # open alpha, back to the Home, open beta
            await click(pg, f'#home .home-row[data-id="{aid}"] .name')
            await pg.until(f"{CURRENT} === {json.dumps(aid)}", 10)
            await click(pg, "#home nav.nav-pill .nav-home")
            await pg.until(f"{CURRENT} === 'home'", 10)
            await click(pg, f'#home .home-row[data-id="{bid}"] .name')
            r["two_pages_beta_shown"] = bool(await pg.until(
                f"document.querySelectorAll('#home nav.nav-pill .nav-page').length === 2 && {CURRENT} === {json.dumps(bid)}", 10))
            r["frames_loaded"] = await frames_loaded(pg, [urls[aid], urls[bid]])
            born_js = ("JSON.stringify(Object.fromEntries([...document.querySelectorAll('#home .page[data-id] iframe.canvas-frame')]"
                       ".map(f=>[f.closest('.page').dataset.id, f.dataset.born])))")
            born = json.loads(await pg.js(born_js) or "{}")
            info["born"] = born
            r["two_frames"] = sorted(born) == sorted([aid, bid])
            pg.navigations = []
            # every state from here on: a frame dropped (not reloaded) would follow
            # a state where its row was not running at its url
            log = await StateLog(rig.home_port).__aenter__()

            # switch: the pill, then mod+2, then mod+1; mod+0 the Home
            await click(pg, f'#home nav.nav-pill .nav-page[data-id="{aid}"]')
            r["pill_to_alpha"] = await selection_is(aid, "pill → alpha")
            await key(pg, "2")
            r["mod2_to_beta"] = await selection_is(bid, "mod+2 → beta")
            await key(pg, "1")
            r["mod1_to_alpha"] = await selection_is(aid, "mod+1 → alpha")
            await key(pg, "0")
            r["mod0_home"] = await selection_is(None, "mod+0 → the Home") and bool(await pg.js(f"!!{q('#home .nav-home[aria-current=page]')}"))
            await asyncio.sleep(1.0)  # a late reload would show by now
            await pg.js("1")  # drain the events that arrived meanwhile
            info["navigations"] = list(pg.navigations)
            r["no_child_navigation"] = pg.navigations == []
            r["frames_kept"] = json.loads(await pg.js(born_js) or "{}") == born

            # a second tab shows the agent's selection, and moves it for both
            async with rig.page() as pg2:
                r["tab2_same_current"] = bool(await pg2.until(f"{CURRENT} === 'home'", 10))
                await key(pg2, "1")
                r["tab2_moves_tab1"] = bool(await pg.until(f"{CURRENT} === {json.dumps(aid)}", 10)) and \
                    bool(await pg2.until(f"{CURRENT} === {json.dumps(aid)}", 10))
                info["page_errors_tab2"] = page_errors(pg2)[:5]
            born_after = json.loads(await pg.js(born_js) or "{}")
            await log.__aexit__()
            info["born_after_tab2"] = born_after
            info["navigations_after_tab2"] = list(pg.navigations)
            info["states_seen"] = len(log.states)
            # every state in which an open page's row was not running at its first url
            info["page_rows_not_running"] = [
                {"seq": st["seq"], **{i: st["rows"].get(i) for i in (aid, bid)}} for st in log.states
                if any(st["rows"].get(i) != ["running", urls[i]] for i in (aid, bid))
            ][:10]
            r["frames_still_kept"] = born_after == born and pg.navigations == []

            # mod+W closes the shown page (alpha); alpha keeps running
            await key(pg, "w")
            r["alpha_page_closed"] = bool(await pg.until(f"!{q(f'#home .nav-page[data-id={json.dumps(aid)}]')}"
                                                         f" && !{q(f'#home .page[data-id={json.dumps(aid)}]')}", 10))
            state = await rig.home_call({"type": "list"})
            r["alpha_not_a_page"] = aid not in state.get("pages", [])
            r["alpha_runs"] = lock_pid(alpha) is not None and http_get(urls[aid])[0] == 200
            info["page_errors"] = page_errors(pg)[:5]
            r["no_page_errors"] = not info["page_errors"] and not info["page_errors_tab2"]

            # the host restarts with beta selected: the pages and selection come back
            await click(pg, f'#home nav.nav-pill .nav-page[data-id="{bid}"]')
            r["beta_selected"] = await selection_is(bid, "beta selected before the restart")
            rig.restart_home()
            pg.logs.clear()
            await rig.open_home(pg)
            r["listed_after_restart"] = bool(await pg.until("!!document.querySelector('#home .home-row')", 20))
            r["beta_current_after_restart"] = bool(await pg.until(
                f"!!{q(f'#home nav.nav-pill .nav-page[data-id={json.dumps(bid)}][aria-current=page]')}", 20))
            state = await rig.home_call({"type": "list"})
            info["after_restart"] = {"pages": state.get("pages"), "selection": state.get("selection")}
            url = (row_of(state, bp).get("canvas") or {}).get("url") or urls[bid]
            r["beta_frame_after_restart"] = await frames_loaded(pg, [url])
            r["alpha_page_stays_closed"] = aid not in state.get("pages", []) and not await pg.js(
                f"!!{q(f'#home .nav-page[data-id={json.dumps(aid)}]')}")
            r["alpha_still_runs"] = lock_pid(alpha) is not None and http_get(urls[aid])[0] == 200
            info["page_errors_after_restart"] = page_errors(pg)[:5]
            r["no_page_errors_after_restart"] = not info["page_errors_after_restart"]

    try:
        asyncio.run(drive())
    finally:
        rig.stop_all()
    ok = bool(r) and all(r.values())
    rig.summary["checks"], rig.summary["info"] = r, info
    note = "switched five times with no reload, two tabs one selection, mod+W closed a page, a restart kept the pages" if ok else \
        f"failed: {', '.join(k for k, v in r.items() if not v)} · {json.dumps(info)[:4000]}"
    return verdict(*rig.finish(ok, note))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
