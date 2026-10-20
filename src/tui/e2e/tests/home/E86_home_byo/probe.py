"""E86 home_byo — the reskin contract's example client and its limits.

1. The example page: with `alpha` running and `beta` stopped, Chrome opens
   `http://127.0.0.1:<home port>/canvas_page/file/list.html`; it shows both
   rows with their statuses.
2. Driving it: beta's Start reaches `running` within 30 s; its Stop reaches
   `stopped` within 20 s, and beta's registry entry has `pid: null` and
   `canvas: null`.
3. A native client and the `Origin` rule:
   - `<home>/app/home.yaml` names the host's pid and the port the probe used;
   - a websocket to `/user/ws` with no `Origin` header gets an answer to `list`
     with both rows;
   - an upgrade with `Origin: http://example.invalid` gets 403;
   - a `user.forward` to `kernel` `update_agent` on `web` (adding an
     `allowed_origins`) is refused `unauthorized`."""

from __future__ import annotations

import asyncio
import json
import secrets
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.home_live import HomeRig, canonical, click, page_errors, q, verdict, yaml_of  # noqa: E402

FOREIGN = "http://example.invalid"


def row_of(state: dict, path: str) -> dict:
    return next((x for x in state.get("rows", []) if x.get("path") == path), {})


async def native_call(port: int, target: str, payload: dict) -> dict:
    """One `user.forward` the way a native client sends it: no Origin header."""
    import websockets

    frame = {"type": "call", "target": "user", "id": secrets.token_hex(8),
             "payload": {"type": "forward", "target": target, "payload": payload}}
    async with websockets.connect(f"ws://127.0.0.1:{port}/user/ws") as ws:
        await ws.send(json.dumps(frame))
        while True:
            m = json.loads(await asyncio.wait_for(ws.recv(), 10))
            if m.get("type") in ("reply", "error") and m.get("id") == frame["id"]:
                return m.get("data", m)


async def upgrade_status(port: int, origin: str) -> int | str:
    """The HTTP status of a websocket upgrade to `/user/ws` sent with `origin`
    (101 when it is accepted)."""
    import websockets

    try:
        async with websockets.connect(f"ws://127.0.0.1:{port}/user/ws", origin=origin):
            return 101
    except Exception as e:  # noqa: BLE001 — the refusal is the measurement
        response = getattr(e, "response", None)
        status = getattr(response, "status_code", None) or getattr(e, "status_code", None)
        return status if isinstance(status, int) else f"{type(e).__name__}: {e}"


def main() -> bool:
    rig = HomeRig(__file__)
    rig.build()
    r: dict = {}
    info: dict = {}

    async def drive() -> None:
        rig.start_home()
        alpha = await rig.fantastic("alpha")
        beta = await rig.fantastic("beta", running=False)
        ap, bp = canonical(alpha), canonical(beta)
        state = await rig.until_state("alpha running, beta stopped",
                                      lambda s: row_of(s, ap).get("status") == "running" and row_of(s, bp).get("status") == "stopped", 30)
        aid, bid = row_of(state, ap)["id"], row_of(state, bp)["id"]
        list_url = f"http://127.0.0.1:{rig.home_port}/canvas_page/file/list.html"

        def status(rid: str) -> str:
            sel = '#rows tr[data-id="' + rid + '"] td.status'
            return q(sel) + "?.dataset.status"

        async with rig.page() as pg:
            # 1. the example page
            await pg.open(list_url)
            r["both_rows"] = bool(await pg.until(f"{status(aid)} === 'running' && {status(bid)} === 'stopped'", 20))
            info["page_text"] = (await pg.js("document.body.innerText") or "")[:600]
            r["protocol_checked"] = not bool(await pg.js("document.getElementById('status').textContent.includes('not started')"))
            # 2. driving it
            r["start_clicked"] = await click(pg, f'#rows tr[data-id="{bid}"] button[data-intent=start]')
            r["running"] = bool(await pg.until(f"{status(bid)} === 'running'", 30))
            r["stop_clicked"] = await click(pg, f'#rows tr[data-id="{bid}"] button[data-intent=stop]')
            r["stopped"] = bool(await pg.until(f"{status(bid)} === 'stopped'", 20))
            cleared = False
            reg: dict = {}
            for _ in range(100):
                reg = rig.registry(bid) or {}
                if reg and reg.get("pid") is None and reg.get("canvas") is None:
                    cleared = True
                    break
                await asyncio.sleep(0.2)
            info["registry_stopped"] = reg
            r["registry_cleared"] = cleared
            info["clicks"] = pg.click_log
            info["page_errors"] = page_errors(pg)[:5]
            r["no_page_errors"] = not page_errors(pg)
        # 3. a native client, and the Origin rule
        run = yaml_of(rig.home / "app" / "home.yaml")
        info["run_file"] = run
        r["run_file_names_the_host"] = run.get("pid") == rig.home_proc.pid and run.get("port") == rig.home_port
        listed = await native_call(rig.home_port, "home", {"type": "list", "version": 1})
        info["native_list"] = [(x.get("name"), x.get("status")) for x in listed.get("rows", [])]
        r["native_admitted"] = listed.get("type") == "state" and {ap, bp} <= {x.get("path") for x in listed.get("rows", [])}
        code = await upgrade_status(rig.home_port, FOREIGN)
        info["foreign_origin"] = code
        r["foreign_origin_403"] = code == 403
        widen = await native_call(rig.home_port, "kernel", {"type": "update_agent", "id": "web", "allowed_origins": [FOREIGN]})
        info["widen"] = widen
        r["cannot_widen"] = widen.get("reason") == "unauthorized"
        r["still_403"] = await upgrade_status(rig.home_port, FOREIGN) == 403

    try:
        asyncio.run(drive())
    finally:
        rig.stop_all()
    ok = bool(r) and all(r.values())
    rig.summary["checks"], rig.summary["info"] = r, info
    note = "list.html lists and drives start/stop; no Origin admitted, a foreign one 403, the socket cannot widen it" if ok else \
        f"failed: {', '.join(k for k, v in r.items() if not v)} · {json.dumps(info)[:4000]}"
    return verdict(*rig.finish(ok, note))


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
