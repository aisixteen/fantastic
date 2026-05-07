"""ssh_runner — remote `fantastic serve` lifecycle.

Each ssh_runner agent represents one project on one remote host.
Verbs exec ssh-as-subprocess to control the remote kernel and
maintain a local SSH tunnel so the browser / canvas iframe can
reach the remote webapp at `http://localhost:<local_port>/`.

Record fields (set on create_agent):
  host         — ssh alias / hostname (passed to `ssh <host>`)
  remote_path  — project root on the remote box
  remote_cmd   — absolute path to the remote `fantastic` CLI
                 (e.g. /home/me/.venv/bin/fantastic)
  remote_port  — port the remote `serve` binds (REQUIRED, no default)
  local_port   — local port the SSH tunnel forwards from
                 (used by `get_webapp` so canvas can iframe)
  entry_path   — URL suffix appended to the local tunnel for
                 `get_webapp` (e.g. "<canvas_webapp_id>/" so the
                 iframe lands directly on the remote canvas)

Verbs:
  reflect   — identity + every field above + live status
  boot      — no-op (no auto-start; explicit `start` keeps remote
              control intentional)
  shutdown  — alias for `stop`; called by core.delete_agent's
              universal lifecycle hook
  start     — SSH → `cd <remote_path> && nohup <remote_cmd> serve
              --port <remote_port> &`. Polls remote
              `.fantastic/lock.json` to confirm liveness, then
              opens the local SSH tunnel.
  stop      — kill local tunnel (TERM, 2s, KILL); SSH read remote
              pid from lock.json + kill. Idempotent.
  restart   — stop + start.
  status    — {tunnel_alive, remote_alive, http_ok, remote_pid}
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
import urllib.error
import urllib.request
from dataclasses import dataclass

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
    options as kernel_bridge — ExitOnForwardFailure makes ssh exit
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


def _http_health(local_port: int) -> bool:
    try:
        with urllib.request.urlopen(
            f"http://localhost:{local_port}/_kernel/reflect", timeout=2
        ) as resp:
            return resp.status == 200
    except (urllib.error.URLError, OSError, TimeoutError):
        return False


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + every record field + live status. No args."""
    rec = kernel.get(id) or {}
    st = _state(id)
    return {
        "id": id,
        "sentence": "Remote `fantastic serve` lifecycle over SSH.",
        "host": rec.get("host"),
        "remote_path": rec.get("remote_path"),
        "remote_cmd": rec.get("remote_cmd"),
        "remote_port": rec.get("remote_port"),
        "local_port": rec.get("local_port"),
        "entry_path": rec.get("entry_path", ""),
        "tunnel_pid": st.tunnel_pid,
        "tunnel_alive": (st.tunnel_proc is not None and st.tunnel_proc.poll() is None),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, kernel):
    """No-op. ssh_runner does NOT auto-start the remote — `start` is explicit, so a kernel restart doesn't unintentionally boot every remote."""
    return None


async def _start(id, payload, kernel):
    """No args. SSHs to `<host>`, runs `cd <remote_path> && nohup <remote_cmd> serve --port <remote_port> > .fantastic/serve.log 2>&1 &`, polls the remote `.fantastic/lock.json` to confirm liveness, then opens the local SSH tunnel `-L <local_port>:localhost:<remote_port>`. Returns {started:bool, remote_pid, tunnel_pid} on success or {error} on failure."""
    rec = kernel.get(id) or {}
    host = rec.get("host")
    rp = rec.get("remote_path")
    cmd = rec.get("remote_cmd")
    rport_val = rec.get("remote_port")
    lport = rec.get("local_port")
    if not (host and rp and cmd and lport and rport_val):
        return {
            "error": "ssh_runner.start: host, remote_path, remote_cmd, remote_port, local_port all required"
        }
    rport = int(rport_val)
    lport = int(lport)

    # Kick off the remote serve. nohup + & detaches so the SSH
    # connection can close while serve keeps running. mkdir -p the
    # .fantastic dir for the log redirect target.
    rp_q = shlex.quote(rp)
    cmd_q = shlex.quote(cmd)
    remote = (
        f"cd {rp_q} && mkdir -p .fantastic && "
        f"nohup {cmd_q} serve --port {rport} "
        f"> .fantastic/serve.log 2>&1 &"
    )
    rc, out, err = await _ssh_exec(host, remote, timeout=15.0)
    if rc != 0:
        return {
            "error": f"ssh_runner.start: ssh failed (rc={rc}): {err.strip() or out.strip()}"
        }

    # Poll the remote lock.json to confirm serve actually came up.
    lock_path = f"{rp_q}/.fantastic/lock.json"
    deadline = asyncio.get_event_loop().time() + REMOTE_LOCK_POLL_TIMEOUT
    remote_pid: int | None = None
    while asyncio.get_event_loop().time() < deadline:
        rc2, out2, _ = await _ssh_exec(
            host, f"cat {lock_path} 2>/dev/null", timeout=5.0
        )
        if rc2 == 0 and out2.strip():
            try:
                lock = json.loads(out2)
                remote_pid = int(lock.get("pid"))
                break
            except (json.JSONDecodeError, TypeError, ValueError):
                pass
        await asyncio.sleep(REMOTE_LOCK_POLL_INTERVAL)
    if remote_pid is None:
        return {
            "error": "ssh_runner.start: remote serve did not write lock.json in time"
        }

    # Open local tunnel.
    st = _state(id)
    if st.tunnel_proc is not None and st.tunnel_proc.poll() is None:
        # Idempotent — already tunneling.
        return {
            "started": True,
            "remote_pid": remote_pid,
            "tunnel_pid": st.tunnel_pid,
            "already_tunneled": True,
        }
    try:
        tunnel = await _open_tunnel(host, lport, rport)
    except Exception as e:
        return {
            "error": f"ssh_runner.start: tunnel failed: {e}",
            "remote_pid": remote_pid,
        }
    st.tunnel_proc = tunnel
    st.tunnel_pid = tunnel.pid
    return {"started": True, "remote_pid": remote_pid, "tunnel_pid": tunnel.pid}


async def _stop(id, payload, kernel):
    """No args. Kills the local SSH tunnel (TERM, 2s, KILL); SSHs to the host, reads remote pid from `.fantastic/lock.json`, SIGTERMs it. Idempotent: missing tunnel / missing remote pid is OK."""
    rec = kernel.get(id) or {}
    host = rec.get("host")
    rp = rec.get("remote_path")
    if not (host and rp):
        return {"error": "ssh_runner.stop: host + remote_path required"}

    st = _state(id)
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


async def _restart(id, payload, kernel):
    """No args. stop + start. Returns the start reply."""
    await _stop(id, payload, kernel)
    return await _start(id, payload, kernel)


async def _status(id, payload, kernel):
    """No args. {tunnel_alive, remote_alive, http_ok, remote_pid}. http_ok is a 2s probe to `http://localhost:<local_port>/_kernel/reflect` through the tunnel — proves end-to-end liveness."""
    rec = kernel.get(id) or {}
    host = rec.get("host")
    rp = rec.get("remote_path")
    lport = rec.get("local_port")
    st = _state(id)

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
                    host, f"kill -0 {remote_pid} 2>/dev/null && echo ok", timeout=5.0
                )
                remote_alive = rc2 == 0
            except (json.JSONDecodeError, TypeError, ValueError):
                pass
    http_ok = bool(lport) and tunnel_alive and _http_health(int(lport))
    return {
        "tunnel_alive": tunnel_alive,
        "remote_alive": remote_alive,
        "remote_pid": remote_pid,
        "http_ok": http_ok,
    }


async def _shutdown(id, payload, kernel):
    """Lifecycle hook. Same as stop — called by core.delete_agent before record removal so the tunnel + remote serve don't outlive the agent."""
    return await _stop(id, payload, kernel)


async def _get_webapp(id, payload, kernel):
    """No args. Canvas-facing UI descriptor: {url, default_width, default_height, title}. The url points at the LOCAL tunnel + entry_path so the canvas iframes the remote webapp transparently."""
    rec = kernel.get(id) or {}
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
        return {"error": f"ssh_runner: unknown type {t!r}"}
    return await fn(id, payload, kernel)
