#!/usr/bin/env python3
"""Boot the fantastic OS image in QEMU and check the kiosk (Phase 9, #54).

    uv run --project src/lib/python python os/ci/boot_check.py \
        --image dist/os/fantastic-os_<v>.raw --arch x86-64|arm64 --out os-check \
        [--timeout 600] [--step 90]

`--timeout` bounds the boot, up to the app's run file; `--step` bounds each later
wait. What it proves, in order (the steps are numbered as in the issue):

  3  the run file names a live `fantastic-app`, whose environment has SHELL (the
     home starts each canvas in the login shell's environment); logind has a
     `wayland` session on seat0/tty1, active; the app announced its home
  4  the home kernel's loopback port, forwarded out of the guest
  5  the kernel answers `home` `list` over /user/ws (`fantastic.home` v1)
  6  the Home page, in the app's window, answers a key press: Ctrl+0 goes in
     through QEMU, and `select {id: null}` comes out of the page through the
     kernel (this check never sends it itself; only the Home page turns Ctrl+0
     into that call)
  7  the screen shows something (QMP screendump, not uniform)
  8  a canvas started from the kiosk runs on the bundled sidecar, and the screen
     changes when its page slides in
  9  SIGTERM to the app: the kiosk comes back by itself, the canvas keeps its pid
     and is adopted, and 5-7 pass again

The QEMU parts (`Vm`, the channels, the /user/ws client) are `qemu.py`'s, shared with
`e2e_system.py`. Everything here is stdlib plus `websockets` (imported in the socket
helpers only, so the pure helpers import anywhere: see test_boot_check.py). It
imports nothing from `e2e/`.
"""

from __future__ import annotations

import argparse
import asyncio
import http.client
import json
import re
import sys
import time
from pathlib import Path
from urllib.parse import urlsplit

# the QEMU side, and what this file's steps use of it; the names are re-exported
# (container_check.py and the tests reach them as `boot_check.<name>`)
from qemu import (  # noqa: F401
    HOME_FORWARD,
    SYNC_EVERY,
    Chan,
    Fail,
    Vm,
    forward_call,
    http_get,
    log,
    parse_ppm,
    qemu_argv,
    reply_data,
    retrying,
    set_log_file,
    until,
    vars_firmware,
    ws_connect,
    ws_errors,
)

KIOSK_USER = "fantastic"
KIOSK_HOME = f"/home/{KIOSK_USER}"
RUN_FILE = f"{KIOSK_HOME}/.fantastic/app/home.yaml"
CANVAS_DIR = f"{KIOSK_HOME}/boot-check"
# the first screen of a canvas-less kiosk differs from a canvas page by at least this
MIN_CHANGED = 0.01
# the step now running; main() puts it in the one failure line
STEP = "0"


def at(step: str) -> None:
    global STEP
    STEP = step


# ── pure helpers (unit-tested) ───────────────────────────────────────────────


def is_uniform(pixels: bytes) -> bool:
    """Every RGB pixel equal to the first (a blank screen)."""
    if len(pixels) % 3 or not pixels:
        raise ValueError(f"{len(pixels)} bytes are not RGB pixels")
    return pixels == pixels[:3] * (len(pixels) // 3)


def changed_fraction(a: tuple[int, int, bytes], b: tuple[int, int, bytes]) -> float:
    """The fraction of pixels that differ between two same-sized images."""
    if a[:2] != b[:2]:
        raise ValueError(f"images differ in size: {a[:2]} vs {b[:2]}")
    pa, pb = a[2], b[2]
    changed = sum(
        1
        for r, g, bl, r2, g2, b2 in zip(pa[0::3], pa[1::3], pa[2::3], pb[0::3], pb[1::3], pb[2::3])
        if (r, g, bl) != (r2, g2, b2)
    )
    return changed / (a[0] * a[1])


def parse_run_file(text: str) -> dict:
    """`{pid, port, url}` from the app's flat run file; all three keys required."""
    out: dict = {}
    for line in text.splitlines():
        m = re.match(r"^(pid|port|url): *(.*)$", line)
        if not m:
            continue
        v = m.group(2).strip()
        if len(v) >= 2 and v[0] == v[-1] and v[0] in "'\"":
            v = v[1:-1]
        out[m.group(1)] = v
    missing = [k for k in ("pid", "port", "url") if not out.get(k)]
    if missing:
        raise ValueError(f"the run file has no {', '.join(missing)}: {text!r}")
    try:
        pid, port = int(out["pid"]), int(out["port"])
    except ValueError as e:
        raise ValueError(f"the run file's pid/port are not numbers: {text!r}") from e
    if not out["url"].startswith(f"http://127.0.0.1:{port}/"):
        raise ValueError(f"the run file's url {out['url']!r} is not http://127.0.0.1:{port}/…")
    return {"pid": pid, "port": port, "url": out["url"]}


def is_select_home(frame: object) -> bool:
    """The watcher's frame for the Home page's `select {id: null}` call, exactly."""
    return (
        isinstance(frame, dict)
        and frame.get("type") == "event"
        and frame.get("payload") == {"type": "select", "id": None}
    )


def check_state(data: object) -> dict:
    """`home` `list`'s answer: a `fantastic.home` v1 state, with no error."""
    if not isinstance(data, dict):
        raise Fail(f"home list answered {data!r}, not a state")
    if "error" in data:
        raise Fail(f"home list answered an error: {data['error']!r}")
    if (data.get("type"), data.get("protocol"), data.get("version")) != ("state", "fantastic.home", 1):
        raise Fail(f"home list answered type/protocol/version {data.get('type')!r}/{data.get('protocol')!r}/{data.get('version')!r}")
    return data


def find_row(state: dict, path: str) -> dict | None:
    return next((r for r in state.get("rows", []) if r.get("path") == path), None)


def parse_props(text: str) -> dict[str, str]:
    """`systemctl show` / `loginctl show-session` output: `Key=value` lines."""
    return dict(line.split("=", 1) for line in text.splitlines() if "=" in line)


def check_session(props: dict[str, str]) -> None:
    """The kiosk's logind session: wayland, user class, seat0, tty1, active."""
    want = {"Type": "wayland", "Class": "user", "Seat": "seat0", "TTY": "tty1", "Active": "yes"}
    bad = {k: props.get(k) for k, v in want.items() if props.get(k) != v}
    if bad:
        raise Fail(f"the logind session is {bad}, wanted {want}")


def check_shell(environ: dict[str, str]) -> str:
    """The app's SHELL: the home cannot start a canvas without it."""
    shell = environ.get("SHELL")
    if shell != "/bin/bash":
        raise Fail(f"the app's SHELL is {shell!r}, not /bin/bash (the home cannot start a canvas without it)")
    return shell


def parse_environ(raw: bytes) -> dict[str, str]:
    """`/proc/<pid>/environ`: NUL-separated `NAME=value`."""
    out = {}
    for item in raw.split(b"\0"):
        if b"=" in item:
            k, v = item.split(b"=", 1)
            out[k.decode("utf-8", "replace")] = v.decode("utf-8", "replace")
    return out


async def home_call(port: int, payload: dict, timeout: float = 10) -> object:
    """A call to `home` the way a page makes one: through `user`."""
    return await forward_call(port, "home", payload, timeout)


async def key_reaches_home(vm: Vm, port: int, step: float) -> None:
    """Ctrl+0 every 2 s until the `home` watcher sees the Home page's `select {id: null}`."""
    async with ws_connect(port, "/home/ws") as ws:
        end = time.monotonic() + step
        while True:
            vm.alive()
            vm.send_keys("ctrl", "0")
            until = time.monotonic() + 2
            while time.monotonic() < until:
                try:
                    raw = await asyncio.wait_for(ws.recv(), until - time.monotonic())
                except TimeoutError:
                    break
                if is_select_home(json.loads(raw)):
                    return
            if time.monotonic() > end:
                raise Fail(f"Ctrl+0 never reached `home` as select {{id: null}} within {step:g} s")


def read_run_file(vm: Vm) -> dict | None:
    """The app's run file, or None while the guest has none."""
    code, out, _ = vm.ga_run(["/bin/cat", RUN_FILE])
    return parse_run_file(out) if code == 0 else None


def page_served(get, port: int, path: str, marker: str) -> bool:
    """`get(port, path)` answers 200 with `marker` in the body. A refused or
    half-closed connection is not yet served (the server is still coming up)."""
    try:
        status, body = get(port, path)
    except (OSError, http.client.HTTPException):
        return False
    return status == 200 and marker in body


def wait_page(vm: Vm, run: dict, step: float) -> None:
    """The run file's url, through the forward: 200 and the Home document."""
    path = urlsplit(run["url"]).path or "/"
    until(
        vm,
        f"GET {run['url']} through the forward (200, data-page=\"home\")",
        lambda: page_served(http_get, run["port"], path, 'data-page="home"'),
        step,
    )


def check_kernel(vm: Vm, run: dict, step: float) -> dict:
    """Step 5: `home` `list` over /user/ws answers a `fantastic.home` v1 state."""

    def listed():
        try:
            return asyncio.run(home_call(run["port"], {"type": "list"}))
        except ws_errors() as e:
            retrying("home list", e)  # the forward or the kernel is not up yet
            return None

    state = check_state(until(vm, "home list over /user/ws", listed, step))
    log(f"step 5: home list answers fantastic.home v1 (seq {state.get('seq')}, {len(state.get('rows', []))} rows)")
    return state


def check_keys(vm: Vm, run: dict, step: float) -> None:
    """Step 6: Ctrl+0 through QEMU reaches `home` as the Home page's `select {id: null}`."""
    try:
        asyncio.run(key_reaches_home(vm, run["port"], step))
    except ws_errors() as e:
        raise Fail(f"the /home/ws watcher: {type(e).__name__}: {e}") from e
    log("step 6: Ctrl+0 reached home as the Home page's select {id: null}")


def check_screen(vm: Vm, name: str) -> tuple[int, int, bytes]:
    """Step 7: the screen is not uniform (and kept as <name>.ppm / .png)."""
    img = vm.screendump(name)
    if is_uniform(img[2]):
        raise Fail(f"{name}: the screen is one uniform colour ({img[0]}x{img[1]})")
    log(f"step 7: {name} is {img[0]}x{img[1]}, not uniform")
    return img


def comm(vm: Vm, pid: int) -> str:
    code, out, _ = vm.ga_run(["/bin/cat", f"/proc/{pid}/comm"])
    return out.strip() if code == 0 else f"(no process {pid})"


# ── the steps ────────────────────────────────────────────────────────────────


def check_session_and_app(vm: Vm, run: dict, step: float) -> None:
    """Step 3.2-3.4: the process, SHELL, the logind session, the app's own lines."""
    pid = run["pid"]
    if (c := comm(vm, pid)) != "fantastic-app":
        raise Fail(f"/proc/{pid}/comm reads {c!r}, not fantastic-app")
    # The home starts each canvas in the login environment its $SHELL prints: a
    # system unit with User= gets SHELL from passwd (useradd --shell /bin/bash).
    code, raw, _ = vm.ga_run(["/bin/cat", f"/proc/{pid}/environ"])
    if code != 0:
        raise Fail(f"/proc/{pid}/environ unreadable")
    shell = check_shell(parse_environ(raw.encode()))
    log(f"step 3: pid {pid} is fantastic-app with SHELL={shell}")

    sid = vm.sh(f"loginctl show-user {KIOSK_USER} -p Display --value").strip()
    if not sid:
        raise Fail(f"logind: {KIOSK_USER} has no display session")
    props = parse_props(vm.sh(f"loginctl show-session {sid} -p Type -p Class -p Seat -p TTY -p Active"))
    check_session(props)
    log(f"step 3: logind session {sid}: {props}")

    def announced() -> bool:
        journal = vm.sh("journalctl -b -o cat --no-pager").splitlines()
        return f"fantastic-app: the home: {run['url']}" in journal and "fantastic-app: the backdrop: none" in journal

    until(vm, "the journal's `fantastic-app: the home:` and `the backdrop: none` lines", announced, step)
    log("step 3: the app announced its home and its backdrop")


def check_canvas(vm: Vm, run: dict, home_img, step: float) -> int:
    """Step 8: a canvas opened from the kiosk runs on the sidecar; the screen follows."""
    vm.sh(f"runuser -u {KIOSK_USER} -- mkdir -p {CANVAS_DIR}")
    try:
        opened = asyncio.run(home_call(run["port"], {"type": "open", "paths": [CANVAS_DIR]}))
    except ws_errors() as e:
        raise Fail(f"home open {CANVAS_DIR}: {type(e).__name__}: {e}") from e
    if isinstance(opened, dict) and "error" in opened:
        raise Fail(f"home open refused {CANVAS_DIR}: {opened['error']}")

    def running():
        try:
            row = find_row(check_state(asyncio.run(home_call(run["port"], {"type": "list"}))), CANVAS_DIR)
        except ws_errors() as e:
            retrying("home list", e)
            return None
        return row if row and row.get("status") == "running" and isinstance(row.get("pid"), int) else None

    row = until(vm, f"{CANVAS_DIR} listed running with a pid", running, step)
    pid = row["pid"]
    if (c := comm(vm, pid)) != "fantastic":
        raise Fail(f"the canvas pid {pid}'s comm is {c!r}, not fantastic (the app's sidecar)")
    log(f"step 8: {CANVAS_DIR} runs as pid {pid} (fantastic, the sidecar)")
    time.sleep(5)
    img = vm.screendump("screen-canvas")
    frac = changed_fraction(home_img, img)
    if frac < MIN_CHANGED:
        raise Fail(f"screen-canvas differs from screen-home in {frac:.2%} of its pixels, wanted at least {MIN_CHANGED:.0%}")
    log(f"step 8: the screen changed with the canvas page: {frac:.1%} of the pixels")
    return pid


def check_restart(vm: Vm, run: dict, canvas_pid: int, step: float) -> None:
    """Step 9: SIGTERM to the app; the kiosk, the canvas and the page checks come back."""
    at("9, restart")
    n0 = int(parse_props(vm.sh("systemctl show -p NRestarts fantastic-kiosk.service"))["NRestarts"])
    vm.sh(f"kill -TERM {run['pid']}")
    log(f"step 9: SIGTERM to the app (pid {run['pid']}); NRestarts was {n0}")

    def renewed():
        r = read_run_file(vm)
        return r if r and r["pid"] != run["pid"] else None

    new = until(vm, "a new run file (another pid) after SIGTERM", renewed, step)
    n1 = int(parse_props(vm.sh("systemctl show -p NRestarts fantastic-kiosk.service"))["NRestarts"])
    if n1 != n0 + 1:
        raise Fail(f"NRestarts is {n1}, wanted {n0 + 1}")
    if vm.ga_run(["/bin/sh", "-c", f"test ! -e /proc/{run['pid']}"])[0] != 0:
        raise Fail(f"the old app (pid {run['pid']}) is still there")
    if (c := comm(vm, new["pid"])) != "fantastic-app":
        raise Fail(f"the new pid {new['pid']}'s comm is {c!r}, not fantastic-app")
    log(f"step 9: the kiosk came back: pid {new['pid']}, NRestarts {n1}")

    at("9, after the restart: 4")
    vm.forward(new["port"], HOME_FORWARD)
    wait_page(vm, new, step)
    at("9, after the restart: 5")
    check_kernel(vm, new, step)
    at("9, after the restart: canvas adopted")
    # the canvas outlived the kiosk (KillUserProcesses=no), and the new app adopted it
    try:
        state = check_state(asyncio.run(home_call(new["port"], {"type": "list"})))
    except ws_errors() as e:
        raise Fail(f"home list after the restart: {type(e).__name__}: {e}") from e
    row = find_row(state, CANVAS_DIR)
    if not row or row.get("status") != "running" or row.get("pid") != canvas_pid:
        raise Fail(f"after the restart {CANVAS_DIR} is {row}, wanted running with pid {canvas_pid}")
    log(f"step 9: the canvas still runs as pid {canvas_pid}, adopted by the new app")
    at("9, after the restart: 6")
    check_keys(vm, new, step)
    at("9, after the restart: 7")
    check_screen(vm, "screen-restart")


def diagnostics(vm: Vm) -> None:
    """Step 10, on failure only: what the guest says, kept beside the screenshots."""
    for name, script in {
        "journal.txt": "journalctl -b --no-pager",
        "kiosk.txt": "systemctl status fantastic-kiosk --no-pager",
        "sessions.txt": "loginctl list-sessions --no-pager",
        "home.txt": f"ls -laR {KIOSK_HOME}/.fantastic/app",
        "canvas-logs.txt": f"tail -n +1 {KIOSK_HOME}/.fantastic/logs/* {CANVAS_DIR}/.fantastic/logs/*",
        # what is still alive, and where it waits: step 9 found the run file gone
        # but cage up and no restart, with nothing in the journal to say why
        "processes.txt": "ps -eo pid,ppid,stat,wchan:32,etime,args --forest",
        "cgroups.txt": "systemd-cgls --no-pager; loginctl session-status --no-pager",
        # who still holds a connection to cage (its Wayland socket's peers)
        "sockets.txt": (
            "ss -xp; for p in $(pgrep -f 'cage|fantastic'); do [ \"$p\" = $$ ] && continue;"
            " echo \"== $p $(tr '\\0' ' ' < /proc/$p/cmdline)\"; ls -l /proc/$p/fd; done"
        ),
        "stacks.txt": (
            "for p in $(pgrep -f 'fantastic-app|WebKit|cage'); do [ \"$p\" = $$ ] && continue;"
            " echo \"== $p $(tr '\\0' ' ' < /proc/$p/cmdline)\";"
            " grep -E '^(State|Threads|SigCgt|SigBlk|SigPnd|ShdPnd):' /proc/$p/status;"
            " for t in /proc/$p/task/*; do echo \"-- $t $(cat $t/comm) $(cat $t/wchan)\"; done;"
            " done"
        ),
    }.items():
        try:
            _, out, err = vm.ga_run(["/bin/sh", "-c", script])
            (vm.out / name).write_text(out + err, encoding="utf-8")
        except (Fail, OSError) as e:
            log(f"diagnostics: {name}: {e}")


def run_check(vm: Vm, timeout: float, step: float) -> None:
    at("1")
    log("step 1: starting QEMU")
    vm.start()
    at("2")
    log("step 2: waiting for the guest agent")
    vm.wait_agent(timeout)
    log("step 2: the guest agent answers")

    at("3")
    run = until(vm, f"the run file {RUN_FILE}", lambda: read_run_file(vm), timeout)
    log(f"step 3: run file: {run}")
    check_session_and_app(vm, run, step)

    at("4")
    vm.forward(run["port"], HOME_FORWARD)
    wait_page(vm, run, step)
    log("step 4: the Home document is served through the forward")
    at("5")
    check_kernel(vm, run, step)
    at("6")
    check_keys(vm, run, step)
    at("7")
    home_img = check_screen(vm, "screen-home")

    at("8")
    canvas_pid = check_canvas(vm, run, home_img, step)
    check_restart(vm, run, canvas_pid, step)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--image", type=Path, required=True)
    ap.add_argument("--arch", choices=["x86-64", "arm64"], required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--timeout", type=float, default=600, help="the boot, up to the run file (s)")
    ap.add_argument("--step", type=float, default=90, help="each later wait (s)")
    a = ap.parse_args()
    if not a.image.is_file():
        print(f"boot_check: no image at {a.image}", file=sys.stderr)
        return 2
    a.out.mkdir(parents=True, exist_ok=True)
    set_log_file(a.out / "boot_check.log")
    vm = Vm(a.image.resolve(), a.arch, a.out.resolve())
    try:
        run_check(vm, a.timeout, a.step)
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
