"""THE INSTALLER SMOKE: an INSTALLED desktop app boots its kernel, and its bundled
sidecar starts a canvas. Stdlib only; `installers.yml`'s verify jobs run it on
every OS, on a fresh runner (and in `debian:trixie-slim` for the kiosk).

    python3 smoke.py --root <empty dir> -- <command that starts the app> [args…]

The folder `<root>/alpha` is appended to the command, and the app runs with
`FANTASTIC_HOME=<root>/home`. The command may wrap the app (`xvfb-run -a`, an
AppImage's runtime): the host's run file must then name a descendant of it.

1. Within 60 s the host's run file (`<home>/app/home.yaml`) names the app's pid,
   and its `url` answers HTTP 200.
2. Within 60 s more, `alpha/.fantastic/cli/canvas.yaml` names a live pid whose
   `url` answers 200: the home agent ran the bundled `fantastic canvas`.
3. The app stops. macOS and Linux: SIGTERM, the orderly quit, so the run file
   must be gone within 20 s. Windows: TerminateProcess (`Stop-Process`'s), and the
   pid must be gone within 10 s.
4. The canvas is still alive and answers; then it is stopped (SIGTERM, or
   `taskkill /T /F` on Windows).

Prints `SMOKE: PASS` or `SMOKE: FAIL: <why>` (exit 1) with the app's output.
"""

from __future__ import annotations

import argparse
import os
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

WINDOWS = sys.platform == "win32"
LINUX = sys.platform.startswith("linux")

# 127.0.0.1 only: never through a proxy the environment names
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


class Fail(Exception):
    pass


def say(*a: object) -> None:
    print("[smoke]", *a, flush=True)


def flat_yaml(p: Path) -> dict | None:
    """A flat `key: value` run file, or None while it is absent."""
    try:
        text = p.read_text(encoding="utf-8")
    except (FileNotFoundError, PermissionError):
        return None
    out: dict = {}
    for line in text.splitlines():
        if ":" not in line or line.startswith((" ", "#", "-")):
            continue
        k, v = line.split(":", 1)
        v = v.strip()
        if len(v) >= 2 and v[0] == v[-1] and v[0] in "'\"":
            v = v[1:-1]
        out[k.strip()] = int(v) if v.isdigit() else v
    return out


def proc_stat(pid: int) -> tuple[str, int] | None:
    """Linux: (state, ppid) from /proc, or None when there is no such process."""
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
    except (FileNotFoundError, ProcessLookupError):
        return None
    rest = raw[raw.rindex(")") + 2 :].split()
    return rest[0], int(rest[1])


def ps(pid: int, field: str) -> str | None:
    """macOS: one `ps` field, or None when there is no such process."""
    r = subprocess.run(["ps", "-o", f"{field}=", "-p", str(pid)], capture_output=True, text=True, check=False)
    return r.stdout.strip() or None


def pid_alive(pid: int) -> bool:
    """A live process: a zombie (exited, not yet reaped) is not one."""
    if LINUX:
        st = proc_stat(pid)
        return st is not None and st[0] not in ("Z", "X")
    if not WINDOWS:
        state = ps(pid, "stat")
        return state is not None and not state.startswith("Z")
    import ctypes

    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.OpenProcess.restype = ctypes.c_void_p
    h = k32.OpenProcess(0x1000, False, pid)
    if not h:
        return ctypes.get_last_error() == 5
    code = ctypes.c_ulong()
    ok = k32.GetExitCodeProcess(ctypes.c_void_p(h), ctypes.byref(code))
    k32.CloseHandle(ctypes.c_void_p(h))
    return bool(ok) and code.value == 259


def descends_from(pid: int, root: int) -> bool:
    """`pid` is `root` or one of its descendants (POSIX)."""
    for _ in range(64):
        if pid == root:
            return True
        if pid <= 1:
            return False
        if LINUX:
            st = proc_stat(pid)
            parent = st[1] if st else None
        else:
            got = ps(pid, "ppid")
            parent = int(got) if got else None
        if parent is None:
            return False
        pid = parent
    return False


def http_ok(url: str) -> bool:
    try:
        with OPENER.open(url, timeout=5) as r:
            return r.status == 200
    except (urllib.error.URLError, OSError):
        return False


def until(what: str, check, timeout: float, every: float = 0.5):
    end = time.monotonic() + timeout
    while True:
        got = check()
        if got:
            return got
        if time.monotonic() > end:
            raise Fail(f"{what}: not within {timeout:.0f} s")
        time.sleep(every)


def stop_hard(pid: int) -> None:
    if WINDOWS:
        subprocess.run(["taskkill", "/PID", str(pid), "/T", "/F"], capture_output=True, check=False)
    else:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def smoke(root: Path, command: list[str], log: Path) -> None:
    home = root / "home"
    alpha = root / "alpha"
    run_file = home / "app" / "home.yaml"
    canvas_file = alpha / ".fantastic" / "cli" / "canvas.yaml"
    alpha.mkdir(parents=True)
    env = {**os.environ, "FANTASTIC_HOME": str(home)}

    say("launch:", *command, alpha)
    with open(log, "wb") as out:
        launched = subprocess.Popen([*command, str(alpha)], env=env, stdout=out, stderr=subprocess.STDOUT)
    started: list[int] = []
    try:
        say("1. the host's run file names the app, and its url answers")

        def host() -> dict | None:
            if launched.poll() is not None:
                raise Fail(f"the app exited {launched.returncode} before its run file")
            h = flat_yaml(run_file)
            return h if h and isinstance(h.get("pid"), int) and h.get("url") else None

        h = until(f"the run file {run_file}", host, 60)
        app_pid = h["pid"]
        started.append(app_pid)
        if WINDOWS:
            if app_pid != launched.pid:
                raise Fail(f"the run file names pid {app_pid}, the app is {launched.pid}")
        elif not descends_from(app_pid, launched.pid):
            raise Fail(f"the run file names pid {app_pid}, not the launched {launched.pid} or its child")
        until(f"the home {h['url']} answers 200", lambda: http_ok(h["url"]), 30)
        say(f"   app pid {app_pid}, home {h['url']}")

        say("2. the bundled sidecar started alpha's canvas")

        def canvas() -> dict | None:
            c = flat_yaml(canvas_file)
            return c if c and isinstance(c.get("pid"), int) and c.get("url") else None

        c = until(f"the canvas run file {canvas_file}", canvas, 60)
        started.append(c["pid"])
        until(f"the canvas {c['url']} answers 200", lambda: http_ok(c["url"]), 30)
        say(f"   canvas pid {c['pid']}, {c['url']}")

        def app_gone() -> bool:
            if app_pid == launched.pid:
                return launched.poll() is not None
            return not pid_alive(app_pid)

        if WINDOWS:
            say("3. Stop-Process the app (TerminateProcess): gone within 10 s")
            subprocess.run(["taskkill", "/PID", str(app_pid), "/F"], check=True, capture_output=True)
            until("the app gone", app_gone, 10)
        else:
            say("3. SIGTERM, the orderly quit: the app and its run file gone within 20 s")
            os.kill(app_pid, signal.SIGTERM)
            until("the app gone", app_gone, 20)
            until("the run file removed", lambda: not run_file.exists(), 20)
        try:
            code = launched.wait(20)
        except subprocess.TimeoutExpired:
            raise Fail(f"the launch command {command[0]} outlived the app by 20 s") from None
        say(f"   the launch command exited {code}")

        say("4. the canvas outlives the app, then stops")
        if not pid_alive(c["pid"]):
            raise Fail(f"the canvas {c['pid']} died with the app")
        if not http_ok(c["url"]):
            raise Fail(f"the canvas {c['url']} stopped answering after the app")
        if WINDOWS:
            subprocess.run(["taskkill", "/PID", str(c["pid"]), "/T", "/F"], check=True, capture_output=True)
        else:
            os.kill(c["pid"], signal.SIGTERM)
        until("the canvas gone", lambda: not pid_alive(c["pid"]), 20)
    finally:
        for pid in started:
            if pid_alive(pid):
                stop_hard(pid)
        if launched.poll() is None:
            stop_hard(launched.pid)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", required=True, type=Path, help="an empty or absent directory for home/ and alpha/")
    ap.add_argument("command", nargs=argparse.REMAINDER, help="-- the command that starts the installed app")
    a = ap.parse_args()
    command = a.command[1:] if a.command[:1] == ["--"] else a.command
    if not command:
        ap.error("no command after --")
    root = a.root.resolve()
    if root.exists() and any(root.iterdir()):
        ap.error(f"{root} is not empty")
    root.mkdir(parents=True, exist_ok=True)
    log = root / "app.log"
    try:
        smoke(root, command, log)
    except Fail as e:
        print(f"SMOKE: FAIL: {e}", flush=True)
        print("--- the app's output ---", flush=True)
        print(log.read_text(errors="replace")[-8000:] if log.exists() else "(none)", flush=True)
        return 1
    print("SMOKE: PASS", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
