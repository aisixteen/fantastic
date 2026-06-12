"""ssh_runner — remote `fantastic --port N` lifecycle.

Each ssh_runner agent represents one project on one remote host.
Verbs exec ssh-as-subprocess to control the remote kernel and
maintain a local SSH tunnel so the browser / canvas iframe can
reach the remote webapp at `http://localhost:<local_port>/`.

Record fields (set on create_agent):
  host         — ssh alias / hostname (passed to `ssh <host>`)
  remote_path  — project root on the remote box
  remote_cmd   — absolute path to the remote `fantastic` CLI
                 (e.g. /home/me/.venv/bin/fantastic)
  remote_port  — port the remote daemon binds (REQUIRED, no default)
  local_port   — local port the SSH tunnel forwards from
                 (used by `get_webapp` so canvas can iframe)
  entry_path   — URL suffix appended to the local tunnel for
                 `get_webapp` (e.g. "<html_agent_id>/" so the
                 iframe lands directly on the remote canvas)

This bundle is a thin transport over `runner_core`: `SSHTransport`
implements the ssh/tunnel seam; the shared lifecycle bodies live in
`runner_core.core`. Each verb handler builds an `SSHTransport` from the
agent record per-call and delegates to core.

Verbs:
  reflect   — identity + every field above + live status
  boot      — no-op (no auto-start; explicit `start` keeps remote
              control intentional)
  shutdown  — alias for `stop`; called by kernel_state.delete_agent's
              universal lifecycle hook
  start     — SSH → `cd <remote_path> && nohup <remote_cmd> --port <port>
              --port <remote_port> &`. Polls remote
              `.fantastic/lock.json` to confirm liveness, then
              opens the local SSH tunnel.
  stop      — kill local tunnel (TERM, 2s, KILL); SSH read remote
              pid from lock.json + kill. Idempotent.
  restart   — stop + start.
  status    — {tunnel_alive, remote_alive, ws_ok, remote_pid}
  get_webapp — canvas-facing UI descriptor pointing at the local
               tunnel: {url, default_width, default_height, title}

Pure subprocess SSH (no paramiko). Authentication is whatever
`ssh <host>` works as in the user's shell — keys, ssh-agent, and
~/.ssh/config all apply transparently.
"""

from __future__ import annotations

import asyncio
import json
import os
import shlex
import signal as signal_mod
import subprocess
from dataclasses import dataclass

from runner_core import core
from runner_core.health import _ws_health
from runner_core.transport import Transport

REMOTE_LOCK_POLL_TIMEOUT = 30.0
REMOTE_LOCK_POLL_INTERVAL = 0.5
TUNNEL_READY_TIMEOUT = 5.0


@dataclass
class _RunnerState:
    tunnel_proc: subprocess.Popen | None = None
    tunnel_pid: int | None = None


# Process-memory state per agent (mirrors terminal_backend._procs).
_runners: dict[str, _RunnerState] = {}


def _state(id: str) -> _RunnerState:
    s = _runners.get(id)
    if s is None:
        s = _RunnerState()
        _runners[id] = s
    return s


# ─── ssh helpers ────────────────────────────────────────────────


async def _ssh_exec(host: str, cmd: str, timeout: float = 30.0) -> tuple[int, str, str]:
    """Run `ssh <host> '<cmd>'` non-interactively. Returns
    (exit_code, stdout, stderr). Single-quotes the remote command
    via shlex.quote so paths with spaces survive."""
    full = ["ssh", "-o", "BatchMode=yes", host, cmd]
    proc = await asyncio.create_subprocess_exec(
        *full,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        out, err = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except asyncio.TimeoutError:
        proc.kill()
        out, err = await proc.communicate()
        return -1, out.decode(errors="replace"), "ssh timeout"
    return (
        proc.returncode or 0,
        out.decode(errors="replace"),
        err.decode(errors="replace"),
    )


async def _open_tunnel(
    host: str, local_port: int, remote_port: int
) -> subprocess.Popen:
    """Open `ssh -L local:localhost:remote -N <host>` in a fresh
    process group, poll the local port until it accepts. Same
    options as ws_bridge — ExitOnForwardFailure makes ssh exit
    immediately on local-port collision; ServerAliveInterval keeps
    stateful firewalls from silently dropping the tunnel."""
    cmd = [
        "ssh",
        "-N",
        "-L",
        f"{local_port}:localhost:{remote_port}",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "BatchMode=yes",
        host,
    ]
    proc = subprocess.Popen(
        cmd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    # Poll local port readiness — early-exit on ssh failure.
    import socket

    deadline = asyncio.get_event_loop().time() + TUNNEL_READY_TIMEOUT
    while asyncio.get_event_loop().time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"ssh tunnel exited early (code {proc.returncode})")
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                s.settimeout(0.2)
                s.connect(("127.0.0.1", local_port))
                return proc
        except OSError:
            await asyncio.sleep(0.1)
    proc.terminate()
    raise TimeoutError(f"ssh tunnel to {host}:{remote_port} not ready")


def _kill_tunnel(proc: subprocess.Popen | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    try:
        os.killpg(os.getpgid(proc.pid), signal_mod.SIGTERM)
    except OSError:
        try:
            proc.terminate()
        except OSError:
            return
    try:
        proc.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(os.getpgid(proc.pid), signal_mod.SIGKILL)
        except OSError:
            try:
                proc.kill()
            except OSError:
                pass


# ─── transport ──────────────────────────────────────────────────


class SSHTransport(Transport):
    """ssh + `ssh -L` tunnel seam. Built per-call from the agent record;
    process-memory tunnel state lives in the module-level `_runners` table
    keyed by agent id."""

    def __init__(self, id: str, record: dict | None):
        super().__init__(record)
        self.id = id

    @property
    def host(self):
        return self.rec.get("host")

    @property
    def remote_path(self):
        return self.rec.get("remote_path")

    @property
    def remote_cmd(self):
        return self.rec.get("remote_cmd")

    # poll-loop constants — core reads these; remote ssh polling is slower
    # than local, hence the dedicated REMOTE_* names this proxies.
    @property
    def lock_poll_timeout(self) -> float:
        return REMOTE_LOCK_POLL_TIMEOUT

    @property
    def lock_poll_interval(self) -> float:
        return REMOTE_LOCK_POLL_INTERVAL

    @property
    def sentence(self) -> str:
        return "Remote `fantastic --port N` lifecycle over SSH."

    def reflect_fields(self) -> dict:
        st = _state(self.id)
        return {
            "host": self.rec.get("host"),
            "remote_path": self.rec.get("remote_path"),
            "remote_cmd": self.rec.get("remote_cmd"),
            "remote_port": self.rec.get("remote_port"),
            "local_port": self.rec.get("local_port"),
            "entry_path": self.rec.get("entry_path", ""),
            "tunnel_pid": st.tunnel_pid,
            "tunnel_alive": (
                st.tunnel_proc is not None and st.tunnel_proc.poll() is None
            ),
        }

    # ─── live state ──────────────────────────────────────────────

    async def read_lock(self) -> dict | None:
        host = self.rec.get("host")
        rp = self.rec.get("remote_path")
        if not (host and rp):
            return None
        rp_q = shlex.quote(rp)
        rc, out, _ = await _ssh_exec(
            host, f"cat {rp_q}/.fantastic/lock.json 2>/dev/null", timeout=5.0
        )
        if rc == 0 and out.strip():
            try:
                return json.loads(out)
            except json.JSONDecodeError:
                return None
        return None

    async def pid_alive(self, pid: int) -> bool:
        host = self.rec.get("host")
        if not host:
            return False
        rc, _, _ = await _ssh_exec(
            host, f"kill -0 {pid} 2>/dev/null && echo ok", timeout=5.0
        )
        return rc == 0

    async def web_port(self) -> int | None:
        # The remote daemon binds the configured remote_port; no disk
        # discovery — it's fixed config on the record.
        rport = self.rec.get("remote_port")
        try:
            return int(rport) if rport else None
        except (TypeError, ValueError):
            return None

    @property
    def ws_port(self) -> int | None:
        lport = self.rec.get("local_port")
        try:
            return int(lport) if lport else None
        except (TypeError, ValueError):
            return None

    # ─── start ───────────────────────────────────────────────────

    def validate_start(self) -> dict | None:
        rec = self.rec
        if not (
            rec.get("host")
            and rec.get("remote_path")
            and rec.get("remote_cmd")
            and rec.get("local_port")
            and rec.get("remote_port")
        ):
            return {
                "error": "ssh_runner.start: host, remote_path, remote_cmd, remote_port, local_port all required"
            }
        return None

    async def already_running(self) -> dict | None:
        # ssh_runner short-circuits on the tunnel inside finish_start (it
        # must first confirm the remote came up); no pre-bringup shortcut.
        return None

    async def bring_up(self) -> dict | None:
        rec = self.rec
        host = rec.get("host")
        rp = rec.get("remote_path")
        cmd = rec.get("remote_cmd")
        rport = int(rec.get("remote_port"))
        # Which HTTP bundle to bootstrap on the remote — overridable on the
        # record (default the standard web bundle), so it's explicit config,
        # not baked in.
        web_module = rec.get("web_module", "web.tools")

        # Two-step bootstrap on the remote:
        #   1. one-shot `fantastic kernel_state create_agent handler_module=web.tools port=N`
        #      persists the web record (uvicorn task dies with the process,
        #      but the record stays on disk).
        #   2. nohup `fantastic` spawns the daemon — `_default` rehydrates
        #      the persisted web, acquires lock, blocks forever.
        rp_q = shlex.quote(rp)
        cmd_q = shlex.quote(cmd)
        remote = (
            f"cd {rp_q} && mkdir -p .fantastic && "
            f"{cmd_q} kernel_state create_agent handler_module={shlex.quote(web_module)} port={rport} "
            f">/dev/null 2>&1 && "
            f"nohup {cmd_q} > .fantastic/serve.log 2>&1 &"
        )
        rc, out, err = await _ssh_exec(host, remote, timeout=15.0)
        if rc != 0:
            return {
                "error": f"ssh_runner.start: ssh failed (rc={rc}): {err.strip() or out.strip()}"
            }
        return None

    async def finish_start(self, pid: int) -> dict:
        rec = self.rec
        host = rec.get("host")
        lport = int(rec.get("local_port"))
        rport = int(rec.get("remote_port"))
        st = _state(self.id)
        if st.tunnel_proc is not None and st.tunnel_proc.poll() is None:
            # Idempotent — already tunneling.
            return {
                "started": True,
                "remote_pid": pid,
                "tunnel_pid": st.tunnel_pid,
                "already_tunneled": True,
            }
        try:
            tunnel = await _open_tunnel(host, lport, rport)
        except Exception as e:
            return {
                "error": f"ssh_runner.start: tunnel failed: {e}",
                "remote_pid": pid,
            }
        st.tunnel_proc = tunnel
        st.tunnel_pid = tunnel.pid
        return {"started": True, "remote_pid": pid, "tunnel_pid": tunnel.pid}

    def start_timeout_error(self) -> dict:
        return {
            "error": "ssh_runner.start: remote serve did not write lock.json in time"
        }

    # ─── stop ────────────────────────────────────────────────────

    async def stop(self) -> dict:
        rec = self.rec
        host = rec.get("host")
        rp = rec.get("remote_path")
        if not (host and rp):
            return {"error": "ssh_runner.stop: host + remote_path required"}

        st = _state(self.id)
        _kill_tunnel(st.tunnel_proc)
        st.tunnel_proc = None
        st.tunnel_pid = None

        # Read remote pid + kill
        rp_q = shlex.quote(rp)
        rc, out, _ = await _ssh_exec(
            host, f"cat {rp_q}/.fantastic/lock.json 2>/dev/null", timeout=5.0
        )
        remote_pid: int | None = None
        if rc == 0 and out.strip():
            try:
                remote_pid = int(json.loads(out).get("pid"))
            except (json.JSONDecodeError, TypeError, ValueError):
                remote_pid = None
        if remote_pid:
            await _ssh_exec(host, f"kill {remote_pid} 2>/dev/null || true", timeout=5.0)
        return {"stopped": True, "remote_pid": remote_pid}

    # ─── status / get_webapp ─────────────────────────────────────

    async def status(self) -> dict:
        rec = self.rec
        host = rec.get("host")
        rp = rec.get("remote_path")
        lport = rec.get("local_port")
        st = _state(self.id)

        tunnel_alive = st.tunnel_proc is not None and st.tunnel_proc.poll() is None
        remote_alive = False
        remote_pid: int | None = None
        if host and rp:
            rp_q = shlex.quote(rp)
            rc, out, _ = await _ssh_exec(
                host, f"cat {rp_q}/.fantastic/lock.json 2>/dev/null", timeout=5.0
            )
            if rc == 0 and out.strip():
                try:
                    lock = json.loads(out)
                    remote_pid = int(lock.get("pid"))
                    rc2, _, _ = await _ssh_exec(
                        host,
                        f"kill -0 {remote_pid} 2>/dev/null && echo ok",
                        timeout=5.0,
                    )
                    remote_alive = rc2 == 0
                except (json.JSONDecodeError, TypeError, ValueError):
                    pass
        ws_ok = bool(lport) and tunnel_alive and await _ws_health(int(lport))
        return {
            "tunnel_alive": tunnel_alive,
            "remote_alive": remote_alive,
            "remote_pid": remote_pid,
            "ws_ok": ws_ok,
        }

    async def get_webapp(self, id: str) -> dict:
        rec = self.rec
        lport = rec.get("local_port")
        if not lport:
            return {"error": "ssh_runner.get_webapp: local_port required"}
        entry = rec.get("entry_path", "") or ""
        host = rec.get("host") or "remote"
        return {
            "url": f"http://localhost:{lport}/{entry}",
            "default_width": int(rec.get("width") or 800),
            "default_height": int(rec.get("height") or 600),
            "title": rec.get("display_name") or host,
        }


def _transport(id, kernel) -> SSHTransport:
    return SSHTransport(id, kernel.get(id) or {})


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + every record field + live status. No args."""
    return await core.reflect(id, _transport(id, kernel), kernel, VERBS)


async def _boot(id, payload, kernel):
    """No-op. ssh_runner does NOT auto-start the remote — `start` is explicit, so a kernel restart doesn't unintentionally boot every remote."""
    return await core.boot(id, _transport(id, kernel), kernel)


async def _start(id, payload, kernel):
    """No args. SSHs to `<host>`, runs `cd <remote_path> && nohup <remote_cmd> --port <remote_port> > .fantastic/serve.log 2>&1 &`, polls the remote `.fantastic/lock.json` to confirm liveness, then opens the local SSH tunnel `-L <local_port>:localhost:<remote_port>`. Returns {started:bool, remote_pid, tunnel_pid} on success or {error} on failure."""
    return await core.start(id, _transport(id, kernel), kernel)


async def _stop(id, payload, kernel):
    """No args. Kills the local SSH tunnel (TERM, 2s, KILL); SSHs to the host, reads remote pid from `.fantastic/lock.json`, SIGTERMs it. Idempotent: missing tunnel / missing remote pid is OK."""
    return await core.stop(id, _transport(id, kernel), kernel)


async def _restart(id, payload, kernel):
    """No args. stop + start. Returns the start reply."""
    await _stop(id, payload, kernel)
    return await _start(id, payload, kernel)


async def _status(id, payload, kernel):
    """No args. {tunnel_alive, remote_alive, ws_ok, remote_pid}. ws_ok
    is a 2s probe over the WS verb channel (`ws://localhost:<local_port>
    /kernel_state/ws`, reflect frame → reply) through the SSH tunnel — proves
    end-to-end liveness."""
    return await core.status(id, _transport(id, kernel), kernel)


async def on_delete(agent):
    """Cascade hook — same as stop: tear down the SSH tunnel + remote
    serve so they don't outlive the agent record."""
    await _stop(agent.id, {}, agent)


async def _get_webapp(id, payload, kernel):
    """No args. Canvas-facing UI descriptor: {url, default_width, default_height, title}. The url points at the LOCAL tunnel + entry_path so the canvas iframes the remote webapp transparently."""
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
        return {"error": f"ssh_runner: unknown type {t!r}"}
    return await fn(id, payload, kernel)
