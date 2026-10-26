"""E92 lock_overlay — the kiosk lock (#55): who may call it, and what the Home does while it holds.

`system` runs `fantastic canvas` in a folder named `system` (the Home's `systemRow`), `alpha`
is a second canvas for the Home to have a page and a selection. The `lock` agent is made in
the system canvas the way the `system` recipe makes it (`kern/system.rs`): handler
`lock.tools`, under `user`, `callable_by {user: verbs}`. The Home page's origin is added to
the system canvas's `web` (`kern/home_origin` does it only under `canvas system` on Linux).

1. Who may call it (every OS): writing `lock` runs the grant pass, which narrows it
   (`kern/reach.rs` NARROWED): its `callable_by` names `user` and the canvas leg, no AI and
   not the `mcp` leg, while an ordinary agent (`files`) is open to `mcp`. `fantastic send`
   (the `mcp` leg, from the system folder) to `lock` is refused, to `files` it completes.
   A websocket to the system canvas with a foreign `Origin` gets 403 and `web` does not list it.
   `user.forward` to `lock` `state`: its fields, where the handler is registered (Linux).
2. The overlay on the Home (every OS): the lock agent's `state` events are fed through the wire's
   `emit` (the real agent's own events where it is available, part 3). `locked: true` shows
   `.lock-overlay` (not hidden) and every other child of `#home` is `inert`; Esc, mod+0/1/2,
   a click on the nav pill and on a row change neither the overlay nor the home agent's selection;
   a PIN tried is refused (the overlay stays, `.lock-note` says why); `locked: false` takes the
   overlay and the `inert` away.
3. The real lock (where `lock.state.available` is true; else it prints
   `SKIP lock_overlay/overlay: lock agent unavailable: <why>`): `set_pin 4321`, `lock`; the
   overlay shows; a wrong PIN `0000` leaves it and `.lock-note` shows the refusal; the right
   PIN hides it, removes `inert`, and the state event says `locked: false`.
"""

from __future__ import annotations

import asyncio
import json
import re
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlsplit

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.home_live import HomeRig, canonical, click, key, page_errors, q, verdict  # noqa: E402
from canvas import host_call  # noqa: E402

FOREIGN = "http://example.invalid"
SRC = next(p for p in Path(__file__).resolve().parents if (p / "bundles").is_dir()) / "bundles" / "fantastic-system-agents" / "src"
LEG = "canvas_ws"
PIN = "4321"
ESCAPE = {"key": "Escape", "code": "Escape", "windowsVirtualKeyCode": 27}
OVERLAY = ".lock-overlay"
CURRENT = ("(()=>{const b=document.querySelector('#home nav.nav-pill button[aria-current=page]');"
           " return b ? (b.classList.contains('nav-home') ? 'home' : b.dataset.id) : null;})()")
BEHIND = "[...document.querySelector('#home').children].filter(c=>!c.classList.contains('lock-overlay'))"


def lock_facts() -> tuple[str, list[str], str]:
    text = (SRC / "lock.rs").read_text()
    module = re.search(r'HANDLER_MODULE: &str = "([^"]+)"', text).group(1)
    body = re.search(r"VERBS: \[&str; \d+\] = \[(.*?)\];", text, re.S).group(1)
    return module, re.findall(r'"(\w+)"', body), (SRC / "views" / "lock.html").read_text()


def row_of(state: dict, path: str) -> dict:
    return next((x for x in state.get("rows", []) if x.get("path") == path), {})


async def upgrade_status(port: int, origin: str) -> int | str:
    import websockets

    try:
        async with websockets.connect(f"ws://127.0.0.1:{port}/user/ws", origin=origin):
            return 101
    except Exception as e:  # noqa: BLE001 — the refusal is the measurement
        response = getattr(e, "response", None)
        status = getattr(response, "status_code", None) or getattr(e, "status_code", None)
        return status if isinstance(status, int) else f"{type(e).__name__}: {e}"


async def emit_state(port: int, state: dict) -> None:
    """`lock`'s state event, as the wire's `emit` makes it (what the Home's watch reads)."""
    import websockets

    async with websockets.connect(f"ws://127.0.0.1:{port}/user/ws", origin=f"http://127.0.0.1:{port}") as ws:
        await ws.send(json.dumps({"type": "emit", "target": "lock", "payload": {"type": "state", "state": state}}))
        await asyncio.sleep(0.3)


def mcp_send(binary: Path, cwd: Path, env: dict, target: str, payload: dict) -> dict:
    """`fantastic send` from `cwd` (the `mcp` leg): the row it prints, or what it said."""
    p = subprocess.run([str(binary), "send", json.dumps({"target": target, "payload": payload})],
                       cwd=cwd, env=env, capture_output=True, text=True, timeout=60)
    try:
        row = json.loads(p.stdout)
    except ValueError:
        row = {"status": "no row", "stdout": p.stdout[-400:]}
    return {**row, "exit": p.returncode, "stderr": p.stderr[-400:]}


def main() -> bool:
    rig = HomeRig(__file__)
    rig.build()
    r: dict = {}
    info: dict = {}
    module, verbs, view = lock_facts()

    async def drive() -> None:
        rig.start_home()
        system = await rig.fantastic("system")
        alpha = await rig.fantastic("alpha")
        sp, ap = canonical(system), canonical(alpha)
        state = await rig.until_state("system and alpha running",
                                      lambda s: row_of(s, sp).get("status") == "running" and row_of(s, ap).get("status") == "running", 30)
        aid = row_of(state, ap)["id"]
        port = urlsplit(row_of(state, sp)["canvas"]["url"]).port
        home_origins = [f"http://127.0.0.1:{rig.home_port}", f"http://localhost:{rig.home_port}"]
        # the Home's origin may use the system canvas (kern/home_origin, which runs under `canvas system` on Linux)
        widened = await host_call(port, "kernel", {"type": "update_agent", "id": "web", "allowed_origins": home_origins})
        info["widened"] = widened
        r["0 the Home's origin is allowed on the system canvas"] = "error" not in widened
        made = await host_call(port, "kernel", {
            "type": "create_agent", "parent_id": "user", "id": "lock", "handler_module": module, "display_name": "lock",
            "callable_by": {"user": verbs}, "view": {"kind": "html", "bytes": view, "calls": verbs}})
        r["0 lock is made"] = "error" not in made

        # 1. who may call it
        def record(agent: str) -> dict:
            for f in (system / ".fantastic").rglob(f"{agent}/agent.yaml"):
                return yaml.safe_load(f.read_text()) or {}
            return {}

        holders: dict = {}
        files_holders: dict = {}
        for _ in range(50):
            holders = record("lock").get("callable_by") or {}
            files_holders = record("files").get("callable_by") or {}
            if "user" in holders and "mcp" in files_holders:
                break
            await asyncio.sleep(0.2)
        info["lock_callable_by"], info["files_callable_by"] = sorted(holders), sorted(files_holders)
        r["1a the grant pass narrowed lock: user and the canvas leg only"] = set(holders) <= {"user", LEG, "$self"} and "user" in holders
        r["1b an ordinary agent (files) is open to the mcp leg"] = "mcp" in files_holders
        refused = mcp_send(rig.binary, system, rig.env(), "lock", {"type": "state"})
        info["mcp_lock"] = refused
        res = refused.get("result") or {}
        r["1c the mcp leg is refused on lock: unauthorized, naming mcp, the verb and lock"] = (
            refused.get("status") == "failed" and refused.get("exit") == 1 and res.get("reason") == "unauthorized"
            and res.get("error") == "mcp may not call 'state' on lock")
        control = mcp_send(rig.binary, system, rig.env(), "files", {"type": "reflect"})
        info["mcp_files"] = {k: control.get(k) for k in ("status", "exit")}
        r["1d the same call to files completes"] = control.get("status") == "completed" and control.get("exit") == 0
        r["1e a foreign Origin gets 403"] = await upgrade_status(port, FOREIGN) == 403
        web = record("web")
        r["1f web does not list the foreign Origin"] = FOREIGN not in (web.get("allowed_origins") or [])
        st = await host_call(port, "lock", {"type": "state"})
        info["lock_state"] = st
        fields = ("locked", "has_pin", "auto_lock_min", "failures", "retry_at_ms")
        registered = "error" not in st
        if registered:
            r["1g user.forward to lock answers its state fields"] = all(k in st for k in fields)
        else:
            print(f"SKIP lock_overlay/state: the lock handler is not registered on this OS ({str(st.get('error'))[:100]})", flush=True)
        available = registered and st.get("available") is True

        # 2. the overlay on the Home
        async with rig.page() as pg:
            sid = row_of(await rig.home_call({"type": "list"}), sp)["id"]
            await click(pg, f'#home .home-row[data-id="{aid}"] .name')
            await pg.until(f"{CURRENT} === {json.dumps(aid)}", 10)
            sel_before = (await rig.home_call({"type": "list"}))["selection"]
            r["2a alpha is the shown page"] = sel_before == aid
            base = {"has_pin": True, "auto_lock_min": 0, "failures": 0, "retry_at_ms": None}
            await emit_state(port, {**base, "locked": False})
            r["2b unlocked: no overlay shown, nothing inert"] = bool(await pg.until(
                f"!!{q(OVERLAY)} && {q(OVERLAY)}.hidden && {BEHIND}.every(c=>!c.inert)", 20))
            await emit_state(port, {**base, "locked": True})
            r["2c locked: the overlay shows and the rest of #home is inert"] = bool(await pg.until(
                f"!{q(OVERLAY)}.hidden && {BEHIND}.length > 0 && {BEHIND}.every(c=>c.inert)", 20))
            await dismissal_attempts(pg, rig, r, info, aid, sid, sel_before)
            # a wrong PIN: the overlay stays, the note says why
            await pg.js(f"(()=>{{const i={q(OVERLAY + ' input')}; i.focus();}})()")
            await pg.cdp("Input.insertText", text="0000")
            await raw_click(pg, OVERLAY + " button")
            r["2e a wrong PIN: the overlay stays and .lock-note says why"] = bool(await pg.until(
                f"(({q(OVERLAY + ' .lock-note')}?.textContent)||'').length > 0 && !{q(OVERLAY)}.hidden", 15))
            info["note_after_pin"] = await pg.js(f"{q(OVERLAY + ' .lock-note')}?.textContent")
            # only the lock agent's own `locked: false` takes it away
            await emit_state(port, {**base, "locked": False})
            r["2f locked: false takes the overlay and the inert away"] = bool(await pg.until(
                f"{q(OVERLAY)}.hidden && {BEHIND}.every(c=>!c.inert)", 15))

            # 3. the real lock, where the OS has one
            if not available:
                why = st.get("why") if registered else "the handler is not registered on this OS"
                print(f"SKIP lock_overlay/overlay: lock agent unavailable: {why}", flush=True)
            else:
                events = await real_lock(port, pg, r, info)
                info["real_lock_events"] = events
            info["page_errors"] = page_errors(pg)[:5]
            r["9 no page errors"] = not info["page_errors"]

    try:
        asyncio.run(drive())
    finally:
        rig.stop_all()
    ok = bool(r) and all(r.values())
    rig.summary["checks"], rig.summary["info"] = r, info
    note = "lock refuses the mcp leg and a foreign Origin; the overlay holds against keys, clicks and the Home's state" if ok else \
        f"failed: {', '.join(k for k, v in r.items() if not v)} · {json.dumps(info, default=str)[:4000]}"
    return verdict(*rig.finish(ok, note))


async def raw_click(pg, sel: str) -> bool:
    """A mouse click at the middle of `sel`, whatever covers it (the overlay does)."""
    rect = await pg.js(f"(()=>{{const e={q(sel)}; if(!e) return null; const r=e.getBoundingClientRect(); return [r.x+r.width/2, r.y+r.height/2];}})()")
    if not rect:
        return False
    await pg.mouse("mousePressed", rect[0], rect[1])
    await pg.mouse("mouseReleased", rect[0], rect[1])
    return True


async def dismissal_attempts(pg, rig, r: dict, info: dict, aid: str, sid: str, sel_before) -> None:
    """Esc, mod+0/1/2, a click on the pill and on a row: the overlay stays, the selection stays."""
    shown = f"!{q(OVERLAY)}.hidden && {BEHIND}.every(c=>c.inert)"
    await pg.js("window.focus()")
    for t in ("keyDown", "keyUp"):
        await pg.cdp("Input.dispatchKeyEvent", type=t, modifiers=0, **ESCAPE)
    r["2d1 Esc does not hide it"] = bool(await pg.js(shown))
    for k in ("0", "1", "2"):
        await key(pg, k)
    await asyncio.sleep(0.5)
    r["2d2 mod+0/1/2 do not hide it"] = bool(await pg.js(shown))
    await raw_click(pg, "#home nav.nav-pill .nav-home")
    await raw_click(pg, f'#home .home-row[data-id="{sid}"] .name')
    await asyncio.sleep(0.5)
    r["2d3 a click on the nav pill and on a row do not hide it"] = bool(await pg.js(shown))
    sel_after = (await rig.home_call({"type": "list"}))["selection"]
    info["selection"] = [sel_before, sel_after]
    r["2d4 the home agent's selection did not change"] = sel_after == sel_before and bool(await pg.js(f"{CURRENT} === {json.dumps(aid)}"))


async def real_lock(port: int, pg, r: dict, info: dict) -> list:
    """Part 3: the real agent. `set_pin`, `lock`, a wrong PIN, the right one."""
    out = await host_call(port, "lock", {"type": "set_pin", "new": PIN})
    r["3a set_pin"] = "error" not in out
    out = await host_call(port, "lock", {"type": "lock"})
    r["3b lock"] = "error" not in out and out.get("locked") is True
    r["3c the overlay shows, the rest is inert"] = bool(await pg.until(f"!{q(OVERLAY)}.hidden && {BEHIND}.every(c=>c.inert)", 20))
    await pg.js(f"{q(OVERLAY + ' input')}.focus()")
    await pg.cdp("Input.insertText", text="0000")
    await raw_click(pg, OVERLAY + " button")
    r["3d a wrong PIN: the overlay stays, the note shows the refusal"] = bool(await pg.until(
        f"(({q(OVERLAY + ' .lock-note')}?.textContent)||'').length > 0 && !{q(OVERLAY)}.hidden", 15))
    await pg.js(f"{q(OVERLAY + ' input')}.focus()")
    await pg.cdp("Input.insertText", text=PIN)
    await raw_click(pg, OVERLAY + " button")
    r["3e the right PIN hides it and the inert goes"] = bool(await pg.until(f"{q(OVERLAY)}.hidden && {BEHIND}.every(c=>!c.inert)", 20))
    after = await host_call(port, "lock", {"type": "state"})
    r["3f the state says locked: false"] = after.get("locked") is False
    await host_call(port, "lock", {"type": "clear_pin", "pin": PIN})
    return [after]


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
