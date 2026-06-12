"""local_runner — `fantastic` lifecycle for local projects.

Each agent represents one project on this machine. Verbs spawn /
signal a `fantastic` subprocess directly (no SSH, no tunnels). The
spawned kernel rehydrates its persisted `web` agent at boot, so
`start` works in two steps: (1) one-shot `kernel_state create_agent
handler_module=web.tools port=<free>` to write the record to disk,
then (2) `subprocess.Popen([cmd])` to spawn the long-running kernel.

Live state is read from two sibling files in the project's
`.fantastic/` dir:

  - `lock.json` — `{pid:int}`, PID-only (substrate's lock).
  - `agents/web_*/agent.json` — the web bundle's persisted record,
    which carries the port (the bundle owns its endpoint info).

Record fields (set on create_agent):
  remote_path  — project root (absolute filesystem path)
  remote_cmd   — `fantastic` CLI to invoke (default: "fantastic" from PATH)
  entry_path   — URL suffix appended to the live serve URL for
                 `get_webapp` (e.g. "<canvas_backend_id>/" so the
                 iframe lands directly on the project's canvas)

This bundle is a thin transport over `runner_core`: `LocalTransport`
implements the filesystem/subprocess seam; the shared lifecycle bodies
live in `runner_core.core`. Each verb handler builds a `LocalTransport`
from the agent record per-call and delegates to core.

Verbs:
  reflect   — identity + every field above + live status
  boot      — no-op (no auto-start; explicit `start` keeps lifecycle intentional)
  shutdown  — alias for `stop`; called by kernel_state.delete_agent's universal
              lifecycle hook
  start     — pick a free port, pre-create the web record, spawn the
              daemon, poll until lock.json appears and the web record
              has a port
  stop      — read remote pid from lock.json, SIGTERM, wait for death
              (escalate to SIGKILL after 6s), remove stale lock file
  restart   — stop + start
  status    — {running, pid, port, ws_ok}
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
from pathlib import Path

from file_bridge import fs
from runner_core import core
from runner_core.health import _ws_health
from runner_core.transport import Transport

LOCK_POLL_TIMEOUT = 30.0
LOCK_POLL_INTERVAL = 0.5
STOP_POLL_TIMEOUT = 6.0
STOP_POLL_INTERVAL = 0.1


def _free_port() -> int:
    # Bind to loopback (not "" / 0.0.0.0) — we never actually listen on
    # the socket, only ask the kernel to allocate a free port number,
    # so the bind interface is functionally irrelevant. Using "" trips
    # CodeQL's py/bind-socket-all-network-interfaces; 127.0.0.1 gives
    # the same allocation behaviour without the security flag.
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _read_lock(remote_path: str) -> dict | None:
    # `remote_path` is a SIBLING project dir (outside this kernel's cwd by design),
    # so the disk read funnels through fs's EXTERNAL surface — clamped within that
    # project, not the running dir.
    if not fs.exists(remote_path, ".fantastic/lock.json", external=True):
        return None
    try:
        return json.loads(
            fs.read_text(remote_path, ".fantastic/lock.json", external=True)
        )
    except (json.JSONDecodeError, OSError, ValueError):
        return None


def _sweep_lock(remote_path: str) -> None:
    """Idempotently remove a stale `lock.json` in a sibling project (ignore if already
    gone — that's idempotency, not a fallback). Routed through fs's external surface."""
    try:
        if fs.exists(remote_path, ".fantastic/lock.json", external=True):
            fs.remove(remote_path, ".fantastic/lock.json", external=True)
    except (OSError, ValueError):
        pass


def _pid_alive(pid: int) -> bool:
    if not pid or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except (OSError, ProcessLookupError):
        return False


def _discover_web_port(remote_path: str) -> int | None:
    """Scan `<remote_path>/.fantastic/agents/*/agent.json` for the first
    record carrying a `port` field (the serve agent, DUCK-TYPED — not by
    bundle name, so any HTTP bundle works); return its `port`. Port lives
    on the serve agent's record, not lock.json (which is PID-only)."""
    if not fs.is_dir(remote_path, ".fantastic/agents", external=True):
        return None
    for entry in fs.list_dir(remote_path, ".fantastic/agents", external=True):
        rel = f".fantastic/agents/{entry.name}/agent.json"
        if not fs.exists(remote_path, rel, external=True):
            continue
        try:
            rec = json.loads(fs.read_text(remote_path, rel, external=True))
        except (json.JSONDecodeError, OSError, ValueError):
            continue
        # Duck-type the serve agent by its `port` field (ANY HTTP bundle that
        # carries a port) rather than a hardcoded handler_module — local_runner
        # reads a spawned project's disk records before it is live, so it cannot
        # reflect, and it must not assume a specific bundle name owns the port.
        p = rec.get("port")
        if isinstance(p, int) and p > 0:
            return p
    return None


def _live_pid_port(remote_path: str) -> tuple[int | None, int | None]:
    """Return (pid, port) iff a live serve is recorded; else (None, None).
    pid comes from lock.json (PID-only); port from the web agent record."""
    lock = _read_lock(remote_path)
    if not lock:
        return None, None
    pid = lock.get("pid")
    if not isinstance(pid, int) or not _pid_alive(pid):
        return None, None
    port = _discover_web_port(remote_path)
    if port is None:
        return None, None
    return pid, port


def _has_web_record(remote_path: str) -> bool:
    """True if any agent.json under `<remote_path>/.fantastic/agents/*/` carries a
    `port` (a serve agent — duck-typed, not by bundle name)."""
    if not fs.is_dir(remote_path, ".fantastic/agents", external=True):
        return False
    for entry in fs.list_dir(remote_path, ".fantastic/agents", external=True):
        rel = f".fantastic/agents/{entry.name}/agent.json"
        if not fs.exists(remote_path, rel, external=True):
            continue
        try:
            rec = json.loads(fs.read_text(remote_path, rel, external=True))
        except (json.JSONDecodeError, OSError, ValueError):
            continue
        if isinstance(rec.get("port"), int) and rec["port"] > 0:
            return True
    return False


# ─── transport ──────────────────────────────────────────────────


class LocalTransport(Transport):
    """Filesystem + `fantastic` subprocess seam. Built per-call from the
    agent record so multiple local projects (and the ssh backend) coexist
    in one kernel with no shared module state."""

    @property
    def remote_path(self) -> str:
        return self.rec.get("remote_path", "")

    @property
    def remote_cmd(self) -> str:
        return self.rec.get("remote_cmd", "fantastic")

    # poll-loop constants — read the module-level names so tests that
    # monkeypatch `local_runner.tools.LOCK_POLL_*` are honoured by core.
    @property
    def lock_poll_timeout(self) -> float:
        return LOCK_POLL_TIMEOUT

    @property
    def lock_poll_interval(self) -> float:
        return LOCK_POLL_INTERVAL

    @property
    def sentence(self) -> str:
        return "Local `fantastic --port N` lifecycle (subprocess + lock.json)."

    def reflect_fields(self) -> dict:
        pid, port = _live_pid_port(self.remote_path)
        return {
            "remote_path": self.rec.get("remote_path"),
            "remote_cmd": self.rec.get("remote_cmd", "fantastic"),
            "entry_path": self.rec.get("entry_path", ""),
            "running": pid is not None,
            "pid": pid,
            "port": port,
        }

    # ─── live state ──────────────────────────────────────────────

    async def read_lock(self) -> dict | None:
        return _read_lock(self.remote_path)

    async def pid_alive(self, pid: int) -> bool:
        return _pid_alive(pid)

    async def web_port(self) -> int | None:
        return _discover_web_port(self.remote_path)

    @property
    def ws_port(self) -> int | None:
        return _discover_web_port(self.remote_path)

    # ─── start ───────────────────────────────────────────────────

    def validate_start(self) -> dict | None:
        rp = self.rec.get("remote_path")
        if not rp:
            return {"error": "local_runner.start: remote_path required"}
        if not fs.is_dir(rp, "", external=True):
            return {"error": f"local_runner.start: not a directory: {rp}"}
        return None

    async def already_running(self) -> dict | None:
        pid, port = _live_pid_port(self.remote_path)
        if pid is not None:
            return {
                "started": True,
                "pid": pid,
                "port": port,
                "already_running": True,
            }
        return None

    async def bring_up(self) -> dict | None:
        rp = self.remote_path
        proj = Path(rp)
        cmd = self.remote_cmd
        port = _free_port()
        self._requested_port = port
        fs.mkdir(rp, ".fantastic", external=True)

        # Step 1: pre-create the web agent record at the chosen port
        # unless one already exists. One-shot subprocess; web's _boot
        # spawns uvicorn and the process exits before binding, but the
        # record persists for the daemon to rehydrate.
        if not _has_web_record(rp):
            subprocess.run(
                [
                    cmd,
                    "kernel_state",
                    "create_agent",
                    "handler_module=web.tools",
                    f"port={port}",
                ],
                cwd=str(proj),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )

        # Step 2: spawn the daemon. `_default` rehydrates the web agent
        # from disk, acquires the lock, blocks while uvicorn serves.
        log = fs.open_append(rp, ".fantastic/serve.log", external=True)
        try:
            subprocess.Popen(
                [cmd],
                cwd=str(proj),
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
                close_fds=True,
            )
        finally:
            log.close()
        return None

    async def finish_start(self, pid: int) -> dict:
        port = _discover_web_port(self.remote_path)
        return {"started": True, "pid": pid, "port": port}

    def start_timeout_error(self) -> dict:
        return {
            "error": "local_runner.start: lock.json never appeared",
            "requested_port": getattr(self, "_requested_port", None),
        }

    # ─── stop ────────────────────────────────────────────────────

    async def stop(self) -> dict:
        rp = self.rec.get("remote_path")
        if not rp:
            return {"error": "local_runner.stop: remote_path required"}
        lock = _read_lock(rp)
        if not lock or not isinstance(lock.get("pid"), int):
            # Nothing to stop, ensure stale file gone.
            _sweep_lock(rp)
            return {"stopped": True, "pid": None}
        pid = lock["pid"]
        try:
            os.kill(pid, signal_mod.SIGTERM)
        except OSError:
            # Already gone.
            _sweep_lock(rp)
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
        _sweep_lock(rp)
        return {"stopped": True, "pid": pid, "died_cleanly": died}

    # ─── status / get_webapp ─────────────────────────────────────

    async def status(self) -> dict:
        rp = self.rec.get("remote_path", "")
        pid, port = _live_pid_port(rp)
        return {
            "running": pid is not None,
            "pid": pid,
            "port": port,
            "ws_ok": bool(port) and await _ws_health(port),
        }

    async def get_webapp(self, id: str) -> dict:
        rp = self.rec.get("remote_path", "")
        pid, port = _live_pid_port(rp)
        if pid is None:
            return {"error": "local_runner.get_webapp: not running"}
        entry = self.rec.get("entry_path", "")
        title = self.rec.get("display_name") or Path(rp).name or id
        return {
            "url": f"http://localhost:{port}/{entry}",
            "default_width": 800,
            "default_height": 600,
            "title": title,
        }


def _transport(id, kernel) -> LocalTransport:
    return LocalTransport(kernel.get(id) or {})


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + every record field + live status. No args."""
    return await core.reflect(id, _transport(id, kernel), kernel, VERBS)


async def _boot(id, payload, kernel):
    """No-op. local_runner does NOT auto-start the project — `start` is explicit so a kernel restart doesn't unintentionally boot every registered project."""
    return await core.boot(id, _transport(id, kernel), kernel)


async def _start(id, payload, kernel):
    """No args. Picks a free port, ensures a `web` agent record exists
    at that port in `<remote_path>/.fantastic/`, then spawns
    `<remote_cmd>` as a detached subprocess in `<remote_path>`. Polls
    `.fantastic/lock.json` until {pid, port} appears (max ~30s).
    Returns {started:bool, pid, port} on success, {error,
    requested_port} on failure (serve.log tail at
    `<remote_path>/.fantastic/serve.log`)."""
    return await core.start(id, _transport(id, kernel), kernel)


async def _stop(id, payload, kernel):
    """No args. SIGTERMs the pid recorded in `.fantastic/lock.json`, polls
    `os.kill(pid, 0)` until the process is gone (max 6s; escalates to
    SIGKILL if still alive), then removes the stale lock file. Idempotent —
    missing lock or already-dead pid returns ok."""
    return await core.stop(id, _transport(id, kernel), kernel)


async def _restart(id, payload, kernel):
    """No args. stop + start. Returns the start reply."""
    await _stop(id, payload, kernel)
    return await _start(id, payload, kernel)


async def _status(id, payload, kernel):
    """No args. {running, pid, port, ws_ok}. ws_ok is a 2s probe over
    the WS verb channel (`ws://localhost:<port>/kernel_state/ws`, reflect frame
    → reply). Proves the kernel is alive AND answering, not just that
    lock.json exists."""
    return await core.status(id, _transport(id, kernel), kernel)


async def on_delete(agent):
    """Cascade hook — same as stop: kill the spawned kernel subprocess
    so it doesn't outlive the agent record."""
    await _stop(agent.id, {}, agent)


async def _get_webapp(id, payload, kernel):
    """No args. Canvas-facing UI descriptor: {url, default_width,
    default_height, title}. URL points at the live local serve
    (`http://localhost:<port>/<entry_path>`). When the project isn't
    running, returns {error} so the canvas skips the frame instead
    of rendering a broken iframe."""
    return await core.get_webapp(id, _transport(id, kernel), kernel)


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
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
