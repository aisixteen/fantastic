"""local_runner — `fantastic serve` lifecycle for local projects.

Each agent represents one project on this machine. Verbs spawn /
signal a `fantastic serve` subprocess directly (no SSH, no tunnels).
Live state is read from the project's own `<remote_path>/.fantastic/
lock.json` — the same file `acquire_serve_lock` writes inside the
spawned kernel — so records carry only identity and the runner
introspects truth from disk + `os.kill(pid, 0)` checks.

Record fields (set on create_agent):
  remote_path  — project root (absolute filesystem path)
  remote_cmd   — `fantastic` CLI to invoke (default: "fantastic" from PATH)
  entry_path   — URL suffix appended to the live serve URL for
                 `get_webapp` (e.g. "<canvas_webapp_id>/" so the
                 iframe lands directly on the project's canvas)

Verbs:
  reflect   — identity + every field above + live status
  boot      — no-op (no auto-start; explicit `start` keeps lifecycle intentional)
  shutdown  — alias for `stop`; called by core.delete_agent's universal
              lifecycle hook
  start     — pick a free port, subprocess.Popen `<remote_cmd> serve
              --port <port>`, poll `<remote_path>/.fantastic/lock.json`
              until {pid, port} appears (or timeout)
  stop      — read remote pid from lock.json, SIGTERM, wait for death
              (escalate to SIGKILL after 6s), remove stale lock file
  restart   — stop + start
  status    — {running, pid, port, http_ok}
  get_webapp — {url, default_width, default_height, title} when alive,
               else {error}; canvas filters errors so dead instances
               don't render a frame
"""

from __future__ import annotations

import asyncio
import json
import os
import signal as signal_mod
import socket
import subprocess
import urllib.error
import urllib.request
from pathlib import Path

LOCK_POLL_TIMEOUT = 30.0
LOCK_POLL_INTERVAL = 0.5
STOP_POLL_TIMEOUT = 6.0
STOP_POLL_INTERVAL = 0.1


def _free_port() -> int:
    s = socket.socket()
    s.bind(("", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _read_lock(remote_path: str) -> dict | None:
    p = Path(remote_path) / ".fantastic" / "lock.json"
    if not p.exists():
        return None
    try:
        return json.loads(p.read_text())
    except (json.JSONDecodeError, OSError):
        return None


def _pid_alive(pid: int) -> bool:
    if not pid or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except (OSError, ProcessLookupError):
        return False


def _http_health(port: int) -> bool:
    try:
        with urllib.request.urlopen(
            f"http://localhost:{port}/_kernel/reflect", timeout=2
        ) as resp:
            return resp.status == 200
    except (urllib.error.URLError, OSError, TimeoutError):
        return False


def _live_pid_port(remote_path: str) -> tuple[int | None, int | None]:
    """Return (pid, port) iff a live serve is recorded; else (None, None)."""
    lock = _read_lock(remote_path)
    if not lock:
        return None, None
    pid = lock.get("pid")
    port = lock.get("port")
    if not isinstance(pid, int) or not _pid_alive(pid):
        return None, None
    if not isinstance(port, int):
        return None, None
    return pid, port


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + every record field + live status. No args."""
    rec = kernel.get(id) or {}
    pid, port = _live_pid_port(rec.get("remote_path", ""))
    return {
        "id": id,
        "sentence": "Local `fantastic serve` lifecycle (subprocess + lock.json).",
        "remote_path": rec.get("remote_path"),
        "remote_cmd": rec.get("remote_cmd", "fantastic"),
        "entry_path": rec.get("entry_path", ""),
        "running": pid is not None,
        "pid": pid,
        "port": port,
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, kernel):
    """No-op. local_runner does NOT auto-start the project — `start` is explicit so a kernel restart doesn't unintentionally boot every registered project."""
    return None


async def _start(id, payload, kernel):
    """No args. Picks a free port, runs `<remote_cmd> serve --port <port>` as a
    detached subprocess in `<remote_path>`, polls `.fantastic/lock.json` until
    {pid, port} appears (max ~30s). Returns {started:bool, pid, port} on
    success, {error, requested_port} on failure (with serve.log tail
    available at `<remote_path>/.fantastic/serve.log`)."""
    rec = kernel.get(id) or {}
    rp = rec.get("remote_path")
    cmd = rec.get("remote_cmd", "fantastic")
    if not rp:
        return {"error": "local_runner.start: remote_path required"}
    proj = Path(rp)
    if not proj.is_dir():
        return {"error": f"local_runner.start: not a directory: {rp}"}

    # Already running?
    pid, port = _live_pid_port(rp)
    if pid is not None:
        return {"started": True, "pid": pid, "port": port, "already_running": True}

    port = _free_port()
    fant_dir = proj / ".fantastic"
    fant_dir.mkdir(parents=True, exist_ok=True)
    log_path = fant_dir / "serve.log"
    log = log_path.open("ab", buffering=0)
    try:
        subprocess.Popen(
            [cmd, "serve", "--port", str(port)],
            cwd=str(proj),
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            close_fds=True,
        )
    finally:
        log.close()

    # Poll lock.json until the spawned kernel writes it.
    deadline = asyncio.get_event_loop().time() + LOCK_POLL_TIMEOUT
    while asyncio.get_event_loop().time() < deadline:
        info = _read_lock(rp)
        if info and info.get("pid") and info.get("port"):
            return {"started": True, "pid": info["pid"], "port": info["port"]}
        await asyncio.sleep(LOCK_POLL_INTERVAL)
    return {
        "error": "local_runner.start: lock.json never appeared",
        "requested_port": port,
    }


async def _stop(id, payload, kernel):
    """No args. SIGTERMs the pid recorded in `.fantastic/lock.json`, polls
    `os.kill(pid, 0)` until the process is gone (max 6s; escalates to
    SIGKILL if still alive), then removes the stale lock file. Idempotent —
    missing lock or already-dead pid returns ok."""
    rec = kernel.get(id) or {}
    rp = rec.get("remote_path")
    if not rp:
        return {"error": "local_runner.stop: remote_path required"}
    lock = _read_lock(rp)
    lock_path = Path(rp) / ".fantastic" / "lock.json"
    if not lock or not isinstance(lock.get("pid"), int):
        # Nothing to stop, ensure stale file gone.
        if lock_path.exists():
            try:
                lock_path.unlink()
            except OSError:
                pass
        return {"stopped": True, "pid": None}
    pid = lock["pid"]
    try:
        os.kill(pid, signal_mod.SIGTERM)
    except OSError:
        # Already gone.
        if lock_path.exists():
            try:
                lock_path.unlink()
            except OSError:
                pass
        return {"stopped": True, "pid": pid, "already_gone": True}
    # Wait for actual death.
    deadline = asyncio.get_event_loop().time() + STOP_POLL_TIMEOUT
    died = False
    while asyncio.get_event_loop().time() < deadline:
        if not _pid_alive(pid):
            died = True
            break
        await asyncio.sleep(STOP_POLL_INTERVAL)
    if not died:
        try:
            os.kill(pid, signal_mod.SIGKILL)
        except OSError:
            pass
        await asyncio.sleep(0.2)
    # Sweep stale lock file.
    if lock_path.exists():
        try:
            lock_path.unlink()
        except OSError:
            pass
    return {"stopped": True, "pid": pid, "died_cleanly": died}


async def _restart(id, payload, kernel):
    """No args. stop + start. Returns the start reply."""
    await _stop(id, payload, kernel)
    return await _start(id, payload, kernel)


async def _status(id, payload, kernel):
    """No args. {running, pid, port, http_ok}. http_ok is a 2s probe to
    `http://localhost:<port>/_kernel/reflect` — proves the serve is
    bound and accepting requests, not just that lock.json exists."""
    rec = kernel.get(id) or {}
    rp = rec.get("remote_path", "")
    pid, port = _live_pid_port(rp)
    return {
        "running": pid is not None,
        "pid": pid,
        "port": port,
        "http_ok": bool(port) and _http_health(port),
    }


async def _shutdown(id, payload, kernel):
    """Lifecycle hook. Same as stop — called by core.delete_agent before record removal so the spawned kernel doesn't outlive the agent."""
    return await _stop(id, payload, kernel)


async def _get_webapp(id, payload, kernel):
    """No args. Canvas-facing UI descriptor: {url, default_width,
    default_height, title}. URL points at the live local serve
    (`http://localhost:<port>/<entry_path>`). When the project isn't
    running, returns {error} so the canvas skips the frame instead
    of rendering a broken iframe."""
    rec = kernel.get(id) or {}
    rp = rec.get("remote_path", "")
    pid, port = _live_pid_port(rp)
    if pid is None:
        return {"error": "local_runner.get_webapp: not running"}
    entry = rec.get("entry_path", "")
    title = rec.get("display_name") or Path(rp).name or id
    return {
        "url": f"http://localhost:{port}/{entry}",
        "default_width": 800,
        "default_height": 600,
        "title": title,
    }


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "shutdown": _shutdown,
    "start": _start,
    "stop": _stop,
    "restart": _restart,
    "status": _status,
    "get_webapp": _get_webapp,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"local_runner: unknown type {t!r}"}
    return await fn(id, payload, kernel)
