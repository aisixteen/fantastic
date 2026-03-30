"""Instance tracking — path-based, disk-truthful."""

import asyncio
import hashlib
import json as _json
import os
from pathlib import Path

from . import _state


# In-memory cache of asyncio.Process objects from launch (for graceful stop).
# Key: instance_id → asyncio.subprocess.Process
_launched_processes: dict[str, asyncio.subprocess.Process] = {}


def _get_own_port() -> int:
    """Get this server's port from config."""
    try:
        return _state._engine.store.get_config().get("port", 0)
    except Exception:
        return 0


def _pid_alive(pid: int) -> bool:
    """Check if a process is alive (works for any PID)."""
    try:
        os.kill(pid, 0)
        return True
    except (ProcessLookupError, PermissionError, OSError):
        return False


def _instance_id(project_dir: str, ssh_host: str = "") -> str:
    """Deterministic ID from path (+ ssh_host for remote)."""
    key = f"{ssh_host}:{project_dir}" if ssh_host else project_dir
    return f"inst_{hashlib.md5(key.encode()).hexdigest()[:6]}"


def _tracked_path() -> Path:
    return _state._engine.project_dir / ".fantastic" / "instances.json"


def _load_tracked() -> list[dict]:
    """Read tracked instances from instances.json."""
    try:
        data = _json.loads(_tracked_path().read_text())
        if isinstance(data, list):
            return data
    except (FileNotFoundError, _json.JSONDecodeError):
        pass
    return []


def _save_tracked(entries: list[dict]) -> None:
    """Write tracked instances to instances.json."""
    _tracked_path().parent.mkdir(parents=True, exist_ok=True)
    _tracked_path().write_text(_json.dumps(entries, indent=2) + "\n")


def _find_tracked(instance_id: str) -> dict | None:
    """Find a tracked entry by instance_id."""
    for e in _load_tracked():
        iid = _instance_id(e["project_dir"], e.get("ssh_host", ""))
        if iid == instance_id:
            return e
    return None


def _add_tracked(entry: dict) -> None:
    """Add or update a tracked entry (keyed by project_dir + ssh_host)."""
    entries = _load_tracked()
    key = (entry["project_dir"], entry.get("ssh_host", ""))
    entries = [e for e in entries if (e["project_dir"], e.get("ssh_host", "")) != key]
    entries.append(entry)
    _save_tracked(entries)


def _remove_tracked(instance_id: str) -> bool:
    """Remove a tracked entry by instance_id. Returns True if found."""
    entries = _load_tracked()
    new = [
        e
        for e in entries
        if _instance_id(e["project_dir"], e.get("ssh_host", "")) != instance_id
    ]
    if len(new) == len(entries):
        return False
    _save_tracked(new)
    return True


async def _fetch_instance_state(entry: dict, *, check_http: bool = False) -> dict:
    """Read fresh pid/port/status from an instance's .fantastic/config.json."""
    project_dir = entry["project_dir"]
    ssh_host = entry.get("ssh_host", "")

    config: dict = {}
    config_rel = ".fantastic/config.json"
    if ssh_host:
        try:
            cmd = f"ssh {ssh_host} 'cat {project_dir}/{config_rel}'"
            proc = await asyncio.create_subprocess_shell(
                cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=5)
            if proc.returncode == 0:
                config = _json.loads(stdout)
        except Exception:
            pass
    else:
        config_path = Path(project_dir) / ".fantastic" / "config.json"
        try:
            if config_path.exists():
                config = _json.loads(config_path.read_text())
        except Exception:
            pass

    pid = config.get("pid", 0)
    port = config.get("port", 0)

    if ssh_host:
        tunnel_pid = entry.get("tunnel_pid", 0)
        alive = _pid_alive(tunnel_pid) if tunnel_pid else bool(pid)
    else:
        alive = _pid_alive(pid) if pid else False

    local_port = entry.get("local_port", port)
    url = f"http://127.0.0.1:{local_port}" if local_port else ""

    # If PID is alive, optionally verify the HTTP API is responsive
    status = "stopped"
    if alive:
        status = "running"
        if check_http and url:
            try:
                import httpx

                async with httpx.AsyncClient() as client:
                    resp = await client.get(f"{url}/api/state", timeout=2)
                    if resp.status_code != 200:
                        status = "unresponsive"
            except Exception:
                status = "unresponsive"

    return {
        "pid": pid,
        "port": port,
        "status": status,
        "url": url,
    }


async def _instance_list(*, check_http: bool = False) -> list[dict]:
    """Build instance list from tracked paths, fetching fresh state from disk."""
    entries = _load_tracked()
    if not entries:
        return []

    states = await asyncio.gather(
        *[_fetch_instance_state(e, check_http=check_http) for e in entries]
    )

    result = []
    for entry, state in zip(entries, states):
        iid = _instance_id(entry["project_dir"], entry.get("ssh_host", ""))
        result.append(
            {
                "id": iid,
                "project_dir": entry["project_dir"],
                "name": entry.get("name", ""),
                "backend": entry.get("backend", "local"),
                **({"ssh_host": entry["ssh_host"]} if entry.get("ssh_host") else {}),
                **state,
            }
        )
    return result


def _instance_list_sync() -> list[dict]:
    """Synchronous version for contexts where we can't await."""
    entries = _load_tracked()
    result = []
    for entry in entries:
        project_dir = entry["project_dir"]
        ssh_host = entry.get("ssh_host", "")
        iid = _instance_id(project_dir, ssh_host)

        pid = 0
        port = 0
        if not ssh_host:
            config_path = Path(project_dir) / ".fantastic" / "config.json"
            try:
                if config_path.exists():
                    config = _json.loads(config_path.read_text())
                    pid = config.get("pid", 0)
                    port = config.get("port", 0)
            except Exception:
                pass
            alive = _pid_alive(pid) if pid else False
        else:
            tunnel_pid = entry.get("tunnel_pid", 0)
            alive = _pid_alive(tunnel_pid) if tunnel_pid else False
            port = entry.get("local_port", 0)

        local_port = entry.get("local_port", port)
        url = f"http://127.0.0.1:{local_port}" if local_port else ""

        result.append(
            {
                "id": iid,
                "project_dir": project_dir,
                "name": entry.get("name", ""),
                "backend": entry.get("backend", "local"),
                **({"ssh_host": ssh_host} if ssh_host else {}),
                "pid": pid,
                "port": port,
                "status": "running" if alive else "stopped",
                "url": url,
            }
        )
    return result
