"""E83 home_list — the Home page lists the home's fantastics.

The fixture: `alpha` running, `beta` registered and then stopped cleanly, and
`gamma` registered and stopped, then its folder renamed away. The Home shows
three `.home-row`s in registry order — each its name, its folder, its status
(running / stopped / missing, labelled), beta's last seen — with the buttons its
actions offer (open is the row's own click). A plain tab (no native shell) has a
path field, and the page stored its root `home_page` through `web_loader`."""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.home_live import HomeRig, canonical, page_errors, q, verdict  # noqa: E402

ROWS = ("[...document.querySelectorAll('#home .home-row')].map(r=>({id:r.dataset.id,"
        " name:r.querySelector('.name')?.textContent, path:r.querySelector('.path')?.textContent, title:r.querySelector('.path')?.title,"
        " status:r.querySelector('.status')?.dataset.status, label:r.querySelector('.status')?.textContent,"
        " seen:r.querySelector('.last-seen')?.textContent ?? '', buttons:[...r.querySelectorAll('button[data-intent]')].map(b=>b.dataset.intent)}))")


def main() -> bool:
    rig = HomeRig(__file__)
    rig.build()
    r: dict = {}
    info: dict = {}

    async def drive() -> None:
        rig.start_home()
        alpha = await rig.fantastic("alpha")
        beta = await rig.fantastic("beta", running=False)
        gamma = await rig.fantastic("gamma", running=False)
        gamma.rename(rig.art / "gamma-gone")
        want = {canonical(alpha): "running", canonical(beta): "stopped", canonical(gamma): "missing"}
        state = await rig.until_state("alpha running, beta stopped, gamma missing",
                                      lambda s: {x["path"]: x["status"] for x in s["rows"]} == want, 30)
        info["state_rows"] = [(x["name"], x["status"], x["actions"]) for x in state["rows"]]
        async with rig.page() as pg:
            r["three_rows"] = bool(await pg.until("document.querySelectorAll('#home .home-row').length === 3"
                                                  " && [...document.querySelectorAll('#home .home-row .status')].map(s=>s.dataset.status).sort().join() === 'missing,running,stopped'", 20))
            rows = await pg.js(ROWS) or []
            info["rows"] = rows
            r["registry_order"] = [x["id"] for x in rows] == rig.registry_order()
            by = {x["name"]: x for x in rows}
            r["names"] = sorted(by) == ["alpha", "beta", "gamma"]
            paths = {"alpha": canonical(alpha), "beta": canonical(beta), "gamma": canonical(gamma)}
            r["paths"] = all(by.get(n, {}).get("path") == p and by[n].get("title") == p for n, p in paths.items())
            r["statuses"] = [by.get(n, {}).get("status") for n in ("alpha", "beta", "gamma")] == ["running", "stopped", "missing"]
            r["labels"] = [by.get(n, {}).get("label") for n in ("alpha", "beta", "gamma")] == ["Running", "Stopped", "Folder not found"]
            r["beta_last_seen"] = (by.get("beta", {}).get("seen") or "").startswith("last seen ")
            r["buttons"] = ([sorted(by.get(n, {}).get("buttons", [])) for n in ("alpha", "beta", "gamma")]
                            == [["remove", "stop"], ["remove", "start"], ["remove"]])
            r["path_field"] = bool(await pg.js(f"!!{q('#home form.open-path input[name=path]')} && !{q('#home button.open-folder')}"))
            r["title"] = await pg.js("document.title") == "Aisixteen Fantastic · home"
            # no problem, no page shown: no toast, no notice and no status capsule on screen
            r["nothing_floats"] = bool(await pg.js("['.problem','.notice','.status-capsule'].every(s=>"
                                                   "getComputedStyle(document.querySelector('#home '+s)).display==='none')"))
            r["home_current"] = bool(await pg.js(f"!!{q('#home nav.nav-pill .nav-home[aria-current=page]')}"))
            # the page stored its root through `web_loader` (`<home>/app/web/`)
            record = rig.home / "app" / "web" / "agents" / "home_page" / "agent.yaml"
            for n in range(100):
                if record.is_file():
                    info["root_record_after_s"] = n / 10
                    break
                await asyncio.sleep(0.1)
            r["root_record"] = record.is_file()
            if record.is_file():
                info["root_record"] = record.read_text()[:300]
            info["page_errors"] = page_errors(pg)[:5]
            r["no_page_errors"] = not info["page_errors"]

    try:
        asyncio.run(drive())
    finally:
        rig.stop_all()
    ok = bool(r) and all(r.values())
    rig.summary["checks"], rig.summary["info"] = r, info
    note = "three rows, in registry order, with their statuses and buttons" if ok else \
        f"failed: {', '.join(k for k, v in r.items() if not v)} · {json.dumps(info)[:1500]}"
    return verdict(*rig.finish(ok, note))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
