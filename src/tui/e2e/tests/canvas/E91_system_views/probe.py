"""E91 system_views — the six system agents' cells (#55), on any OS.

The agents are made by hand with their real handler modules, as the `system`
recipe's `ensure` makes them (`kern/system.rs`): under `user`, `callable_by {user: verbs}`,
`view {kind: html, bytes: <the bundle's view>, calls: verbs}`; each is placed as a host
tile cell (`place {backend_id}`). The OS ports are Linux's, so a fixture state is
pushed through the wire's `emit` (what the agent does itself after a change): the cell
draws it without any OS.

For each of power, audio, network, notifications, displays and lock:
 a) the fixture is drawn (battery 71%, two sinks, the SSIDs, two notifications, an
    output with a mode and a backlight, the lock's auto-lock minutes);
 b) `{available: false, why}` shows the why and no enabled control;
 c) each control driven (audio range and mute, network deactivate and scan,
    notifications dismiss, displays brightness, lock Lock and set auto-lock) makes a
    call on THAT agent, with a verb in that view's `calls`, and on no other agent;
    a control is driven only while the real agent does not report `available: true`
    (off Linux the handler is not even registered: the call is refused; on Linux
    without the service it is `unavailable`): with a real OS behind it nothing is
    clicked, so no test run suspends a machine or mutes its speakers;
 d) the view's source names no target but its own `ID` in `fantastic.send/watch/emit`,
    and the host's log holds no call from the cell to `kernel`, `exec` or `files`;
 e) no page error.
"""

from __future__ import annotations

import asyncio
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from _lib.canvas_live import Rig, verdict  # noqa: E402
from _lib.dev_surface import frame_eval  # noqa: E402

AGENTS_SRC = next(p for p in Path(__file__).resolve().parents if (p / "bundles").is_dir()) / "bundles" / "fantastic-system-agents" / "src"
IDS = ["power", "network", "audio", "displays", "notifications", "lock"]

FIXTURES = {
    "power": {"available": True, "on_battery": True, "lid_closed": False, "sleeping": False,
              "battery": {"percent": 71, "state": "discharging", "time_to_empty_s": 7260, "time_to_full_s": 0, "warning": "none"},
              "can": {"power_off": "yes", "reboot": "yes", "suspend": "yes", "hibernate": "no"}},
    "audio": {"available": True,
              "sinks": [{"id": 71, "name": "alsa_output.a", "description": "E91 Speakers", "volume": 0.7, "muted": False, "default": True},
                        {"id": 72, "name": "alsa_output.b", "description": "E91 Headphones", "volume": 0.25, "muted": False, "default": False}],
              "sources": []},
    "network": {"available": True, "state": "connected_site", "connectivity": "full", "wireless": {"enabled": True, "hardware_enabled": True},
                "devices": [{"iface": "e91-eth0", "kind": "ethernet", "state": "activated", "connection": "E91 wired", "ip4": ["10.0.2.15/24"]}],
                "access_points": [{"ssid": "e91-home", "strength": 80, "security": "wpa-psk", "frequency_mhz": 2412, "active": False, "known": True},
                                  {"ssid": "e91-cafe", "strength": 40, "security": "open", "frequency_mhz": 5180, "active": False, "known": False}],
                "known": [{"uuid": "e91-uuid-1", "id": "E91 wired", "kind": "ethernet", "autoconnect": True}], "busy": False},
    "notifications": {"available": True, "server": "owned", "do_not_disturb": False,
                      "items": [{"id": 901, "app": "e91-app", "summary": "e91 first", "body": "first body", "urgency": "normal", "actions": [], "at_ms": 1, "expires_ms": None},
                                {"id": 902, "app": "e91-app", "summary": "e91 second", "body": "", "urgency": "low", "actions": [], "at_ms": 2, "expires_ms": None}]},
    "displays": {"available": True,
                 "outputs": [{"name": "E91-1", "make": "Acme", "model": "Panel", "enabled": True, "transform": "normal", "scale": 1,
                              "modes": [{"width": 1920, "height": 1080, "refresh_mhz": 60000, "current": True}]}],
                 "backlight": {"name": "e91-bl", "value": 0.5}},
    "lock": {"available": True, "locked": False, "has_pin": True, "auto_lock_min": 17, "failures": 0, "retry_at_ms": 0},
}
#: the rest of an unavailable state, where the agent's own carries more than `why` (power's `can`)
UNAVAILABLE = {"power": {"on_battery": False, "battery": None, "lid_closed": None, "sleeping": False,
                         "can": {v: "unavailable" for v in ("power_off", "reboot", "suspend", "hibernate")}}}
#: what the fixture must show, as [css selector, text it holds] in the cell
SHOWS = {
    "power": [("#battery", "71%"), ("#line", "on battery")],
    "audio": [("#sinks", "E91 Speakers"), ("#sinks", "70%"), ("#sinks", "25%")],
    "network": [("#aps", "e91-home"), ("#aps", "e91-cafe"), ("#devices", "e91-eth0"), ("#known", "E91 wired")],
    "notifications": [("#items", "e91 first"), ("#items", "e91 second"), ("#line", "owned")],
    "displays": [("#outputs", "E91-1"), ("#outputs", "1920x1080")],
    "lock": [("#line", "unlocked"), ("#auto", "min")],
}
#: [css selector clicked or changed, value to set (a range or number), the verb it must call]
CONTROLS = {
    "power": [("#buttons button[data-v=suspend]", None, "suspend")],
    "audio": [("#sinks .row input[type=range]", "0.4", "set_volume"), ("#sinks .row [data-a=mute]", None, "set_mute")],
    "network": [("#devices [data-off]", None, "deactivate"), ("#scan", None, "scan")],
    "notifications": [("#items [data-d]", None, "dismiss")],
    "displays": [("#light input[type=range]", "30", "set_brightness")],
    "lock": [("#lock", None, "lock"), ("#set", None, "set_auto_lock")],
}


def rust_facts(agent: str) -> tuple[str, list[str]]:
    """(handler module, VERBS) from the agent's own source: the same list `ensure` grants and the view `calls`."""
    text = (AGENTS_SRC / f"{agent}.rs").read_text()
    module = re.search(r'HANDLER_MODULE: &str = "([^"]+)"', text).group(1)
    body = re.search(r"VERBS: \[&str; \d+\] = \[(.*?)\];", text, re.S).group(1)
    return module, re.findall(r'"(\w+)"', body)


def view_html(agent: str) -> str:
    return (AGENTS_SRC / "views" / f"{agent}.html").read_text()


def scan_targets(html: str, agent: str) -> list[str]:
    """Every first argument of fantastic.send/watch/emit in the view's source that is not `ID`."""
    assert re.search(rf'const ID = "{agent}";', html), f"{agent}: the view has no `const ID = \"{agent}\"`"
    return [m.group(1).strip() for m in re.finditer(r"fantastic\.(?:send|watch|emit)\(\s*([^,)]+)", html) if m.group(1).strip() != "ID"]


async def emit(port: int, target: str, state: dict) -> None:
    """The agent's own state event, as the wire's `emit` makes it (the page's watch sees it)."""
    import websockets

    async with websockets.connect(f"ws://127.0.0.1:{port}/user/ws", origin=f"http://127.0.0.1:{port}") as ws:
        await ws.send(json.dumps({"type": "emit", "target": target, "payload": {"type": "state", "state": state}}))
        await asyncio.sleep(0.3)


def in_cell(agent: str, expr: str) -> str:
    """`expr` evaluated in the cell whose view names `agent` (any frame; the others answer null)."""
    return ("(()=>{const s=[...document.scripts].find(x=>/const ID = \"/.test(x.textContent));"
            f"const m=s&&s.textContent.match(/const ID = \"(\\w+)\"/); if(!m||m[1]!=={json.dumps(agent)}) return null;"
            f"try{{return ({expr})}}catch(e){{return 'ERR '+e}}}})()")


async def cell_js(pg, agent: str, expr: str):
    for v in await frame_eval(pg, in_cell(agent, expr)):
        if v is not None:
            return v
    return None


async def cell_until(pg, agent: str, expr: str, secs: float = 15) -> bool:
    for _ in range(int(secs * 5)):
        v = await cell_js(pg, agent, expr)
        if v and not (isinstance(v, str) and v.startswith("ERR")):
            return True
        await asyncio.sleep(0.2)
    return False


def host_calls(log: str, agent: str) -> list[tuple[str, str]]:
    """(sender, verb) for every logged call on `agent`."""
    return [(m.group(1) or "", m.group(3)) for m in re.finditer(rf"^\[\d+\] (?:(\S+) → )?(\S*\|)?{re.escape(agent)} call (\w+)", log, re.M)]


def main() -> bool:
    rig = Rig(__file__, live=False)
    rig.build()
    rig.make_home(None)
    rig.init()
    r: dict = {}
    info: dict = {}
    facts = {a: rust_facts(a) for a in IDS}

    async def run() -> None:
        from canvas import host_call

        guid = rig.page_guid()
        for a in IDS:
            module, verbs = facts[a]
            made = await host_call(rig.port, "kernel", {
                "type": "create_agent", "parent_id": "user", "id": a, "handler_module": module, "display_name": a,
                "callable_by": {"user": verbs}, "view": {"kind": "html", "bytes": view_html(a), "calls": verbs}})
            r[f"0 {a} is made"] = "error" not in made
        async with rig.page(1700, 900) as pg:
            view = f"{guid}|view_root"
            for i, a in enumerate(IDS):
                placed = await host_call(rig.port, view, {"type": "place", "backend_id": a, "x": 40 + 520 * (i % 3), "y": 40 + 420 * (i // 3), "width": 500, "height": 400})
                r[f"0 {a} is placed"] = "placed" in placed
        # whether the real agent is a refusal here: only then is a control driven
            real = {a: await host_call(rig.port, a, {"type": "state"}) for a in IDS}
            info["real_available"] = {a: real[a].get("available") for a in IDS}
    
            for a in IDS:
                r[f"1 {a}: the cell's view is mounted"] = await cell_until(pg, a, "document.body.children.length > 0", 30)
            for a in IDS:
                await emit(rig.port, a, FIXTURES[a])
            for a in IDS:
                shown = True
                for sel, text in SHOWS[a]:
                    shown &= await cell_until(pg, a, f"(document.querySelector({json.dumps(sel)})?.textContent||'').includes({json.dumps(text)})", 15)
                r[f"2 {a}: the cell draws the fixture"] = shown
            r["2b audio: two sinks are two rows"] = await cell_js(pg, "audio", "document.querySelectorAll('#sinks .row').length") == 2
            r["2c lock: the auto-lock minutes are in the box"] = await cell_js(pg, "lock", "document.getElementById('min')?.value") == "17"
            r["2d network: both APs and the saved connection are rows"] = (await cell_js(pg, "network", "document.querySelectorAll('#aps .row').length") or 0) >= 3
            # b) unavailable: the why, no enabled control
            for a in IDS:
                why = f"E91 {a} has no OS here"
                await emit(rig.port, a, {**UNAVAILABLE.get(a, {}), "available": False, "why": why})
                ok = await cell_until(pg, a, f"document.body.textContent.includes({json.dumps(why)})", 15)
                live = await cell_js(pg, a, "[...document.querySelectorAll('button:not([disabled]), input, select')].length")
                r[f"3 {a}: unavailable shows its why and no control"] = ok and live == 0
                info[f"3_{a}_controls"] = live
            # c) the controls call their own agent, with their own verbs
            mark = len(rig.log.read_text(errors="replace"))
            others = {a: len(host_calls(rig.log.read_text(errors="replace"), a)) for a in IDS}
            driven = [a for a in IDS if real[a].get("available") is not True]
            info["driven"] = driven
            for a in driven:
                await emit(rig.port, a, FIXTURES[a])
                await cell_until(pg, a, f"(document.querySelector({json.dumps(SHOWS[a][0][0])})?.textContent||'').includes({json.dumps(SHOWS[a][0][1])})", 15)
                for sel, val, verb in CONTROLS[a]:
                    before = len(host_calls(rig.log.read_text(errors="replace"), a))
                    # a redraw (an event) may replace the control: find it again, then act
                    if val is None:
                        act = f"(()=>{{const e=document.querySelector({json.dumps(sel)}); if(!e) return false; e.click(); return true}})()"
                    else:
                        act = (f"(()=>{{const e=document.querySelector({json.dumps(sel)}); if(!e) return false; e.value={json.dumps(val)};"
                               " e.dispatchEvent(new Event('input',{bubbles:true})); e.dispatchEvent(new Event('change',{bubbles:true})); return true})()")
                    clicked = bool(await cell_js(pg, a, act))
                    got: list = []
                    for _ in range(50):
                        got = host_calls(rig.log.read_text(errors="replace"), a)[before:]
                        if got:
                            break
                        await asyncio.sleep(0.2)
                    info[f"4_{a}_{verb}"] = {"clicked": clicked, "calls": got}
                    r[f"4 {a}: {verb} is called on {a}, a verb of its view"] = clicked and any(v == verb for _, v in got) and all(v in facts[a][1] for _, v in got)
                    err = await cell_until(pg, a, "(document.getElementById('err')?.textContent||'').length > 0", 10)
                    r[f"4b {a}: {verb} refused, the cell shows the err line"] = err
                    await emit(rig.port, a, FIXTURES[a])
                    await cell_until(pg, a, f"(document.querySelector({json.dumps(SHOWS[a][0][0])})?.textContent||'').includes({json.dumps(SHOWS[a][0][1])})", 15)
            # d) no other target
            text = rig.log.read_text(errors="replace")
            window = text[mark:]
            counts = {a: len(host_calls(text, a)) for a in IDS}
            gained = {a: counts[a] - others[a] for a in IDS}
            info["5_calls_gained"] = gained
            r["4c a driven cell calls only its own agent"] = all(gained[a] > 0 if a in driven else gained[a] == 0 for a in IDS)
            reached = re.findall(r"^\[\d+\] (?:kernel|exec) call \w+", window, re.M)
            info["5_kernel_exec_calls"] = reached
            for a in IDS:
                extra = scan_targets(view_html(a), a)
                r[f"5 {a}: the view's source names only its own ID"] = not extra
                info[f"5_{a}_other_targets"] = extra
                r[f"5b {a}: the controls made no call on kernel or exec"] = not reached
            info["page_errors"] = [x for x in pg.logs if "WebGL" not in x][:8]
            r["6 no page errors"] = not info["page_errors"]

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
