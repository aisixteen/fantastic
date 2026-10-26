#!/usr/bin/env python3
"""Boot the fantastic OS image in QEMU and check the system agents (Phase 4+9, #55).

    uv run --project src/lib/python python os/ci/e2e_system.py \
        --image dist/os/fantastic-os_<v>.raw --arch x86-64|arm64 --out os-e2e-system \
        [--timeout 600] [--step 90] [--only system,network,audio,notifications,displays]

`--timeout` bounds the boot, up to the kiosk's run file; `--step` bounds each later
wait. The steps are the issue's, in this order (`system` always runs first: the
others need its port):

  system         the kiosk's home started the `system` canvas (`fantastic canvas
                 system`) as the user unit `fantastic-system.service`, not a session
                 scope; the home lists it running with that pid; the six agents
                 (power, network, audio, displays, notifications, lock) answer `reflect`
  audio          one default sink; `set_volume` and `set_mute` reach `wpctl`; a change
                 made by `wpctl` itself arrives as a `state` event on a watch
  network        the QEMU ethernet is `activated` with a 10.0.2.x address; `deactivate`
                 and `activate` reach NetworkManager and each makes a `state` event;
                 with mac80211_hwsim in the guest kernel, the Wi-Fi fixture
                 (`fantastic-ci`) is scanned, joined, and its password is in no state
  notifications  the agent owns org.freedesktop.Notifications (the busctl owner is its
                 pid); a `Notify` call over the user bus shows as a `state` event
  displays       the state is either the outputs `wlr-randr` lists, or `available:
                 false` with a `why`

When the QEMU binary or the image is missing, it prints `SKIP e2e_system: <why>` and
exits 0 (a dev laptop without QEMU). The QEMU parts (`Vm`, the channels, the /user/ws
client) are `qemu.py`'s, shared with `boot_check.py`, whose run-file and home-state
readers are reused. Everything here is stdlib plus `websockets` (imported in the
socket helpers only, so the pure helpers import anywhere: see test_e2e_system.py).
It imports nothing from `e2e/`.
"""

from __future__ import annotations

import argparse
import asyncio
import ipaddress
import json
import re
import shutil
import sys
import time
from pathlib import Path

from boot_check import RUN_FILE, check_state, find_row, parse_run_file
from qemu import (
    HOME_FORWARD,
    SYSTEM_FORWARD,
    Fail,
    Vm,
    forward_call,
    log,
    retrying,
    set_log_file,
    until,
    ws_connect,
    ws_errors,
)

KIOSK_USER = "fantastic"
KIOSK_HOME = f"/home/{KIOSK_USER}"
SYSTEM_DIR = f"{KIOSK_HOME}/system"
SYSTEM_RUN = f"{SYSTEM_DIR}/.fantastic/cli/canvas.yaml"
UNIT = "fantastic-system.service"
AGENTS = ["power", "network", "audio", "displays", "notifications", "lock"]
STEPS = ["system", "audio", "network", "notifications", "displays"]
SSID, PSK = "fantastic-ci", "fantastic-ci-psk"
SUBNET = "10.0.2.0/24"
STEP = "0"
# what the steps share: the system canvas's run file (its port, its pid), set by `system`
SYS: dict = {}


def at(step: str) -> None:
    global STEP
    STEP = step


# ── pure helpers (unit-tested) ───────────────────────────────────────────────


def parse_cgroup(text: str) -> str:
    """The last path element of `/proc/<pid>/cgroup` (cgroup v2: `0::/<path>`): the unit or scope."""
    for line in text.splitlines():
        parts = line.split(":", 2)
        if len(parts) == 3 and parts[0] == "0" and parts[2].strip():
            return parts[2].strip().rstrip("/").rsplit("/", 1)[-1]
    raise ValueError(f"no cgroup v2 line in {text!r}")


def cgroup_is_unit(text: str) -> bool:
    """The process runs in the user's `fantastic-system.service` under `app.slice`: never in a login `session-<n>.scope`."""
    try:
        unit = parse_cgroup(text)
    except ValueError:
        return False
    path = next((ln.split(":", 2)[2].strip() for ln in text.splitlines() if ln.startswith("0::")), "")
    return unit == UNIT and bool(re.fullmatch(r"/user\.slice/user-\d+\.slice/user@\d+\.service/app\.slice/" + re.escape(UNIT), path))


def parse_nmcli_states(text: str) -> dict[str, str]:
    """`nmcli -t -f DEVICE,STATE device`: {device: state}."""
    out = {}
    for line in text.splitlines():
        if ":" in line:
            dev, st = line.split(":", 1)
            out[dev.strip()] = st.strip()
    return out


def parse_wpctl(text: str) -> tuple[float, bool]:
    """`wpctl get-volume`: `Volume: 0.30` or `Volume: 0.30 [MUTED]`."""
    m = re.match(r"\s*Volume:\s*([0-9]+(?:\.[0-9]+)?)(\s+\[MUTED\])?\s*$", text)
    if not m:
        raise ValueError(f"not a wpctl volume line: {text!r}")
    return float(m.group(1)), bool(m.group(2))


def one_default_sink(state: object) -> bool:
    """`audio.state.sinks` has a sink, and exactly one is the default."""
    sinks = state.get("sinks") if isinstance(state, dict) else None
    if not isinstance(sinks, list) or not sinks:
        return False
    return sum(1 for s in sinks if isinstance(s, dict) and s.get("default")) == 1


def audio_ready(state: object) -> bool:
    """An audio state that is `available` with exactly one default sink: a pw-dump read that
    timed out on a slow guest leaves the last sinks beside `available: false`, which is not yet."""
    return isinstance(state, dict) and state.get("available", True) is not False and one_default_sink(state)


def default_volume(state: object) -> float | None:
    """The default sink's volume in an audio state, or None."""
    for s in (state.get("sinks") or []) if isinstance(state, dict) else []:
        if isinstance(s, dict) and s.get("default"):
            return s.get("volume")
    return None


def ip4_in(cidrs: object, subnet: str) -> bool:
    """Some `a.b.c.d/n` of `cidrs` lies in `subnet`."""
    net = ipaddress.ip_network(subnet)
    for c in cidrs if isinstance(cidrs, list) else []:
        try:
            if ipaddress.ip_interface(c).ip in net:
                return True
        except ValueError:
            continue
    return False


def parse_notify_reply(text: str) -> int:
    """`busctl call … Notify` prints `u <id>`; anything else is not a notification id."""
    m = re.fullmatch(r"\s*u\s+(\d+)\s*", text)
    if not m:
        raise ValueError(f"not a Notify reply (`u <id>`): {text!r}")
    return int(m.group(1))


def wifi_row(state: object, ssid: str) -> dict | None:
    """The `access_points` entry for `ssid`."""
    for ap in (state.get("access_points") or []) if isinstance(state, dict) else []:
        if isinstance(ap, dict) and ap.get("ssid") == ssid:
            return ap
    return None


def displays_ok(state: object) -> bool:
    """Outputs listed, or `available: false` with a reason (never a bare unavailable)."""
    if not isinstance(state, dict):
        return False
    if state.get("available"):
        return bool(state.get("outputs"))
    return bool(str(state.get("why") or "").strip())


def choose_steps(only: str | None) -> list[str]:
    """The steps to run, in the issue's order, `system` first. Unknown names are refused."""
    if not only:
        return list(STEPS)
    want = [s.strip() for s in only.split(",") if s.strip()]
    bad = [s for s in want if s not in STEPS]
    if bad:
        raise ValueError(f"unknown step {', '.join(bad)}: choose from {', '.join(STEPS)}")
    return [s for s in STEPS if s == "system" or s in want]


def skip_reason(arch: str, image: Path) -> str | None:
    """Why the QEMU run cannot happen here, or None."""
    binary = "qemu-system-x86_64" if arch == "x86-64" else "qemu-system-aarch64"
    if shutil.which(binary) is None:
        return f"{binary} is not on PATH"
    if not image.is_file():
        return f"no image at {image}"
    return None


# ── the guest and the system canvas ──────────────────────────────────────────


def uid_of(vm: Vm) -> str:
    return vm.sh(f"id -u {KIOSK_USER}").strip()


def as_user(vm: Vm, script: str, timeout: float = 60) -> str:
    """`script` in the guest as the kiosk user, with its runtime dir (the user manager and the user bus)."""
    return vm.sh(f"runuser -u {KIOSK_USER} -- env XDG_RUNTIME_DIR=/run/user/{uid_of(vm)} sh -c {json.dumps(script)}", timeout)


def run_as_user(vm: Vm, script: str, timeout: float = 60) -> tuple[int, str, str]:
    return vm.ga_run(
        ["/bin/sh", "-c", f"runuser -u {KIOSK_USER} -- env XDG_RUNTIME_DIR=/run/user/$(id -u {KIOSK_USER}) sh -c {json.dumps(script)}"], timeout
    )


def sys_call(payload_target: str, payload: dict, timeout: float = 10) -> object:
    """A call to a system agent, through the system canvas's `user`."""
    try:
        return asyncio.run(forward_call(SYS["port"], payload_target, payload, timeout, connect=SYSTEM_FORWARD))
    except ws_errors() as e:
        raise Fail(f"{payload_target} {payload.get('type')}: {type(e).__name__}: {e}") from e


def refused(reply: object) -> str | None:
    return str(reply.get("error")) if isinstance(reply, dict) and "error" in reply else None


def state_of(agent: str) -> dict:
    r = sys_call(agent, {"type": "state"})
    if (e := refused(r)) or not isinstance(r, dict):
        raise Fail(f"{agent} state: {e or r!r}")
    return r


async def watch_events(src: str, act, timeout: float, match) -> dict:
    """Watch `src` over the system canvas's /user/ws, run `act()` (a blocking guest action, off the loop),
    and return the first `event` payload from `src` that `match` accepts."""
    async with ws_connect(SYS["port"], "/user/ws", connect=SYSTEM_FORWARD) as ws:
        await ws.send(json.dumps({"type": "watch", "src": src}))
        await asyncio.sleep(1)  # the watch is set before the action: an event before it is lost
        task = asyncio.get_running_loop().run_in_executor(None, act)
        end = time.monotonic() + timeout
        try:
            while True:
                left = end - time.monotonic()
                if left <= 0:
                    raise Fail(f"no `state` event from `{src}` that matches within {timeout:g} s")
                try:
                    frame = json.loads(await asyncio.wait_for(ws.recv(), left))
                except TimeoutError:
                    continue
                payload = frame.get("payload") if isinstance(frame, dict) else None
                if frame.get("type") == "event" and frame.get("src") == src and isinstance(payload, dict) and match(payload):
                    return payload
        finally:
            await task


def watch(vm: Vm, src: str, act, match, timeout: float = 3) -> dict:
    try:
        return asyncio.run(watch_events(src, act, timeout, match))
    except ws_errors() as e:
        raise Fail(f"watch {src}: {type(e).__name__}: {e}") from e


# ── the steps ────────────────────────────────────────────────────────────────


def read_system_run(vm: Vm) -> dict | None:
    code, out, _ = vm.ga_run(["/bin/cat", SYSTEM_RUN])
    return parse_run_file(out) if code == 0 else None


def check_system(vm: Vm, home_port: int, timeout: float, step: float) -> None:
    run = until(vm, f"the system canvas's run file {SYSTEM_RUN}", lambda: read_system_run(vm), timeout)
    pid, port = run["pid"], run["port"]
    log(f"system: run file {run}")
    code, comm, _ = vm.ga_run(["/bin/cat", f"/proc/{pid}/comm"])
    if code != 0 or comm.strip() != "fantastic":
        raise Fail(f"/proc/{pid}/comm reads {comm.strip()!r}, not fantastic")
    _, cg, _ = vm.ga_run(["/bin/cat", f"/proc/{pid}/cgroup"])
    if not cgroup_is_unit(cg):
        raise Fail(f"pid {pid} runs in {cg.strip()!r}, not user@<uid>.service/app.slice/{UNIT}")
    active = as_user(vm, f"systemctl --user show -p ActiveState --value {UNIT}").strip()
    if active != "active":
        raise Fail(f"{UNIT} is {active!r}, not active")
    log(f"system: pid {pid} is fantastic in {UNIT} (active)")

    vm.forward(port, SYSTEM_FORWARD)
    SYS.update(run)

    def listed():
        try:
            return find_row(check_state(asyncio.run(forward_call(home_port, "home", {"type": "list"}))), SYSTEM_DIR)
        except ws_errors() as e:
            retrying("home list", e)
            return None

    def running():
        row = listed()
        return row if row and row.get("status") == "running" and row.get("pid") == pid else None

    until(vm, f"home lists {SYSTEM_DIR} running with pid {pid}", running, step)
    for agent in AGENTS:
        r = until(vm, f"{agent} reflect", lambda a=agent: _reflect(a), step)
        if (e := refused(r)):
            raise Fail(f"{agent} reflect refused: {e}")
    log(f"system: the home lists {SYSTEM_DIR} running as pid {pid}; {', '.join(AGENTS)} answer reflect")


def _reflect(agent: str):
    try:
        return asyncio.run(forward_call(SYS["port"], agent, {"type": "reflect"}, connect=SYSTEM_FORWARD))
    except ws_errors() as e:
        retrying(f"{agent} reflect", e)
        return None


def wpctl_volume(vm: Vm) -> tuple[float, bool]:
    return parse_wpctl(as_user(vm, "wpctl get-volume @DEFAULT_AUDIO_SINK@").strip())


def check_audio(vm: Vm, step: float) -> None:
    last: list = []

    def ready():
        last[:] = [s := state_of("audio")]
        return audio_ready(s) and s

    try:
        until(vm, "audio.state is available with one default sink", ready, step)
    except Fail as e:
        raise Fail(f"audio.state has no single default sink: {json.dumps(last[0] if last else None)[:400]} ({e})") from e
    for verb, args, want in (("set_volume", {"volume": 0.3}, (0.3, False)), ("set_mute", {"muted": True}, (0.3, True))):
        if (e := refused(sys_call("audio", {"type": verb, **args}))):
            raise Fail(f"audio {verb} refused: {e}")
        got = until(vm, f"wpctl shows {want}", lambda w=want: (v := wpctl_volume(vm)) == w and v, step)
        log(f"audio: {verb} -> wpctl {got}")
    if (e := refused(sys_call("audio", {"type": "set_mute", "muted": False}))):
        raise Fail(f"audio unmute refused: {e}")
    until(vm, "wpctl unmuted", lambda: wpctl_volume(vm)[1] is False, step)
    ev = watch(
        vm,
        "audio",
        lambda: as_user(vm, "wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.5"),
        lambda p: p.get("type") == "state" and default_volume(p.get("state")) == 0.5,
        timeout=15,
    )
    log(f"audio: a wpctl change arrived as a state event (volume {default_volume(ev['state'])})")


def nmcli_states(vm: Vm) -> dict[str, str]:
    return parse_nmcli_states(as_user(vm, "nmcli -t -f DEVICE,STATE device"))


def device(state: dict, kind: str) -> dict | None:
    return next((d for d in state.get("devices") or [] if d.get("kind") == kind), None)


def check_network(vm: Vm, step: float) -> None:
    def wired():
        st = state_of("network")
        d = device(st, "ethernet")
        ok = d and d.get("state") == "activated" and ip4_in(d.get("ip4"), SUBNET) and st.get("state") in ("connected_site", "connected_global")
        return (st, d) if ok else None

    st, eth = until(vm, f"network.state: an activated ethernet in {SUBNET}, connected", wired, step)
    iface = eth["iface"]
    log(f"network: {iface} activated {eth.get('ip4')}, {st['state']}")
    known = next((k for k in st.get("known") or [] if k.get("kind") in ("ethernet", "802-3-ethernet")), None) or (st.get("known") or [None])[0]
    if not known:
        raise Fail(f"network.state has no saved connection to activate again: {json.dumps(st)[:400]}")

    ev = watch(vm, "network", lambda: _expect_ok("network", {"type": "deactivate", "iface": iface}), lambda p: p.get("type") == "state", timeout=10)
    log(f"network: deactivate made a state event ({ev['state'].get('state')})")
    until(vm, f"nmcli says {iface} disconnected", lambda: nmcli_states(vm).get(iface) == "disconnected", 10)
    _expect_ok("network", {"type": "activate", "uuid": known["uuid"]})
    until(vm, f"{iface} activated again", lambda: nmcli_states(vm).get(iface) == "connected", 30)
    until(vm, "network.state: activated again", lambda: (d := device(state_of("network"), "ethernet")) and d.get("state") == "activated", 30)
    log(f"network: activate {known['uuid']} brought {iface} back")
    check_wifi(vm, step)


def _expect_ok(agent: str, payload: dict):
    r = sys_call(agent, payload, 30)
    if (e := refused(r)):
        raise Fail(f"{agent} {payload['type']} refused: {e}")
    return r


def check_wifi(vm: Vm, step: float) -> None:
    code, _, _ = vm.ga_run(["/sbin/modinfo", "mac80211_hwsim"])
    if code != 0:
        log("SKIP network/wifi: no mac80211_hwsim in the guest kernel")
        return
    _expect_ok("network", {"type": "scan"})
    ap = until(
        vm, f"access point {SSID} (wpa-psk)", lambda: (a := wifi_row(state_of("network"), SSID)) and a.get("security") == "wpa-psk" and a, step
    )
    log(f"network: wifi scan found {SSID} ({ap.get('security')}, {ap.get('strength')}%)")
    _expect_ok("network", {"type": "connect", "ssid": SSID, "password": PSK})
    seen: list[str] = []

    def joined():
        st = state_of("network")
        seen.append(json.dumps(st))
        d = device(st, "wifi")
        return d and d.get("state") == "activated" and d

    d = until(vm, "wlan0 activated", joined, 30)
    if d.get("iface") != "wlan0":
        raise Fail(f"the Wi-Fi device is {d.get('iface')!r}, not wlan0")
    if any(PSK in s for s in seen):
        raise Fail("the Wi-Fi password appears in a network state")
    log(f"network: wifi joined {SSID} on wlan0; the password is in none of {len(seen)} states")


def check_notifications(vm: Vm, step: float) -> None:
    st = until(vm, "notifications.state.server owned", lambda: (s := state_of("notifications")) and s.get("server") == "owned" and s, step)
    pid = SYS["pid"]
    status = as_user(vm, "busctl --user status org.freedesktop.Notifications")
    owner = dict(ln.split("=", 1) for ln in status.splitlines() if "=" in ln).get("PID")
    if owner != str(pid):
        raise Fail(f"org.freedesktop.Notifications is owned by pid {owner}, not the canvas pid {pid}")
    log(f"notifications: the name is owned by pid {pid} ({st.get('server')})")
    box: dict = {}

    def notify():
        out = as_user(
            vm, "busctl --user call org.freedesktop.Notifications /org/freedesktop/Notifications org.freedesktop.Notifications "
            "Notify susssasa{sv}i e2e 0 '' e2e-summary e2e-body 0 0 5000"
        )
        box["id"] = parse_notify_reply(out)

    ev = watch(
        vm,
        "notifications",
        notify,
        lambda p: p.get("type") == "state" and any(n.get("summary") == "e2e-summary" for n in (p.get("state") or {}).get("items") or []),
    )
    log(f"notifications: Notify returned id {box['id']} and the state event shows it ({len(ev['state']['items'])} items)")


def check_displays(vm: Vm, step: float) -> None:
    st = until(vm, "displays.state", lambda: state_of("displays"), step)
    if not displays_ok(st):
        raise Fail(f"displays.state is neither outputs nor `available: false` with a why: {json.dumps(st)[:400]}")
    if st.get("available"):
        code, out, err = run_as_user(vm, "WAYLAND_DISPLAY=wayland-0 wlr-randr")
        listed = [ln for ln in out.splitlines() if ln and not ln.startswith(" ")]
        if code != 0 or not listed:
            raise Fail(f"wlr-randr lists no output (exit {code}): {out.strip()} {err.strip()}")
        names = {o.get("name") for o in st["outputs"]}
        if not names & {ln.split()[0] for ln in listed}:
            raise Fail(f"displays.state outputs {sorted(names)} are none of wlr-randr's {listed}")
        log(f"displays: {len(st['outputs'])} output(s) {sorted(names)}, wlr-randr agrees")
    else:
        log(f"displays: unavailable, with a why: {st.get('why')}")


# ── the run ──────────────────────────────────────────────────────────────────


def diagnostics(vm: Vm) -> None:
    """On failure only: what the guest says, kept beside the logs."""
    uid = f"$(id -u {KIOSK_USER})"
    user = f"runuser -u {KIOSK_USER} -- env XDG_RUNTIME_DIR=/run/user/{uid}"
    for name, script in {
        "journal.txt": "journalctl -b --no-pager",
        "system-status.txt": f"{user} systemctl --user status fantastic-system --no-pager",
        "system-journal.txt": f"{user} journalctl --user -u fantastic-system --no-pager",
        "system-logs.txt": f"tail -n +1 {SYSTEM_DIR}/.fantastic/logs/*",
    }.items():
        try:
            _, out, err = vm.ga_run(["/bin/sh", "-c", script])
            (vm.out / name).write_text(out + err, encoding="utf-8")
        except (Fail, OSError) as e:
            log(f"diagnostics: {name}: {e}")


def run_check(vm: Vm, steps: list[str], timeout: float, step: float) -> None:
    at("boot")
    log("starting QEMU (with a sound card)")
    vm.start(audio=True)
    vm.wait_agent(timeout)
    log("the guest agent answers")
    run = until(vm, f"the kiosk's run file {RUN_FILE}", lambda: _home_run(vm), timeout)
    vm.forward(run["port"], HOME_FORWARD)
    log(f"the home: {run}")
    for name in steps:
        at(name)
        log(f"step {name}")
        if name == "system":
            check_system(vm, run["port"], timeout, step)
        else:
            {"audio": check_audio, "network": check_network, "notifications": check_notifications, "displays": check_displays}[name](vm, step)


def _home_run(vm: Vm) -> dict | None:
    code, out, _ = vm.ga_run(["/bin/cat", RUN_FILE])
    return parse_run_file(out) if code == 0 else None


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--image", type=Path, required=True)
    ap.add_argument("--arch", choices=["x86-64", "arm64"], required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--timeout", type=float, default=600, help="the boot, up to the run file (s)")
    ap.add_argument("--step", type=float, default=90, help="each later wait (s)")
    ap.add_argument("--only", help=f"comma-separated steps ({', '.join(STEPS)}); `system` always runs")
    a = ap.parse_args(argv)
    try:
        steps = choose_steps(a.only)
    except ValueError as e:
        print(f"e2e_system: {e}", file=sys.stderr)
        return 2
    if (why := skip_reason(a.arch, a.image)) is not None:
        print(f"SKIP e2e_system: {why}")
        return 0
    a.out.mkdir(parents=True, exist_ok=True)
    set_log_file(a.out / "e2e_system.log")
    vm = Vm(a.image.resolve(), a.arch, a.out.resolve())
    try:
        run_check(vm, steps, a.timeout, a.step)
    except Fail as e:
        log(f"FAIL at step {STEP}: {e}")
        diagnostics(vm)
        return 1
    except Exception as e:  # a bug here is a failure too, with its diagnostics
        log(f"FAIL at step {STEP}: {type(e).__name__}: {e}")
        diagnostics(vm)
        return 1
    finally:
        vm.quit()
    log("PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
