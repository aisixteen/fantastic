"""Instance management tools — launch, stop, list, register, unregister, restart."""

import asyncio
import logging
import os
from typing import Any

from ..dispatch import ToolResult, register_dispatch, register_tool
from ..instance_backend import get_backend
from . import _fire_broadcasts
from . import _instance_tracking as _it

logger = logging.getLogger(__name__)


@register_dispatch("launch_instance")
async def _launch_instance(
    project_dir: str,
    port: int = 0,
    cli: str = "",
    ssh_host: str = "",
    remote_cmd: str = "fantastic",
) -> ToolResult:
    backend = get_backend(ssh_host=ssh_host or None, remote_cmd=remote_cmd)

    try:
        result = await backend.launch(project_dir, port, cli)
    except RuntimeError as e:
        return ToolResult(data={"error": str(e)})

    # Self-tracking guard
    if result.pid == os.getpid():
        return ToolResult(data={"error": "Cannot track self (same PID)"})
    own_port = _it._get_own_port()
    if own_port and result.port == own_port:
        try:
            await backend.stop(result.pid)
        except Exception:
            pass
        return ToolResult(data={"error": "Cannot track self (same port)"})

    abs_dir = (
        os.path.abspath(project_dir) if backend.backend_type == "local" else project_dir
    )
    inst_id = _it._instance_id(abs_dir, ssh_host)

    # Cache process object for graceful stop later
    _it._launched_processes[inst_id] = result.process

    # Build tracked entry
    tracked_entry: dict[str, Any] = {
        "project_dir": abs_dir,
        "name": os.path.basename(abs_dir),
        "backend": backend.backend_type,
    }
    if ssh_host:
        tracked_entry["ssh_host"] = ssh_host
        tracked_entry["remote_cmd"] = remote_cmd
        tracked_entry["tunnel_pid"] = result.pid
        tracked_entry["local_port"] = result.extra.get("local_port", result.port)
    _it._add_tracked(tracked_entry)

    instance_data = {
        "id": inst_id,
        "project_dir": abs_dir,
        "port": result.port,
        "pid": result.pid,
        "url": result.url,
        "backend": backend.backend_type,
        "ssh_host": ssh_host,
        "status": "running",
    }
    return ToolResult(
        data=instance_data,
        broadcast=[
            {"type": "instances_changed", "instances": _it._instance_list_sync()}
        ],
    )


@register_tool("launch_instance")
async def launch_instance(
    project_dir: str,
    port: int = 0,
    cli: str = "",
    ssh_host: str = "",
    remote_cmd: str = "fantastic",
) -> dict:
    """Launch a new Fantastic instance for a project directory.

    Starts a new Fantastic instance process with its own agents.
    Use list_instances() to see tracked instances and stop_instance() to stop them.

    For SSH remote instances, provide ssh_host (uses ~/.ssh/config aliases).
    The instance is accessed via an SSH tunnel — it looks like a local URL.

    Args:
        project_dir: Path to the project directory for the new instance.
        port: Port to use (0 = auto-assign from 49200+).
        cli: Optional CLI command to auto-launch in an agent (e.g. "claude --model sonnet").
        ssh_host: SSH host alias (from ~/.ssh/config) for remote launch. Empty = local.
        remote_cmd: Remote command name for fantastic (default: "fantastic").
    """
    tr = await _launch_instance(project_dir, port, cli, ssh_host, remote_cmd)
    await _fire_broadcasts(tr)
    return tr.data


@register_dispatch("stop_instance")
async def _stop_instance(instance_id: str) -> ToolResult:
    entry = _it._find_tracked(instance_id)
    if entry is None:
        return ToolResult(data={"error": f"Instance {instance_id} not found"})

    ssh_host = entry.get("ssh_host", "")
    backend = get_backend(ssh_host=ssh_host or None)

    # Try graceful stop via cached process object first
    proc = _it._launched_processes.pop(instance_id, None)
    if proc is not None and proc.returncode is None:
        proc.terminate()
        try:
            await asyncio.wait_for(proc.wait(), timeout=3)
        except asyncio.TimeoutError:
            proc.kill()
            await proc.wait()
    else:
        # Fall back to PID-based stop
        if ssh_host:
            tunnel_pid = entry.get("tunnel_pid", 0)
            if tunnel_pid:
                try:
                    await backend.stop(tunnel_pid)
                except Exception as e:
                    logger.warning(f"Error stopping tunnel for {instance_id}: {e}")
        else:
            # Read PID from instance's config
            state = await _it._fetch_instance_state(entry)
            pid = state.get("pid", 0)
            if pid:
                try:
                    await backend.stop(pid)
                except Exception as e:
                    logger.warning(f"Error stopping instance {instance_id}: {e}")

    # For SSH instances: also kill the remote server process
    if ssh_host:
        remote_cmd = entry.get("remote_cmd", "fantastic")
        ssh_backend = get_backend(ssh_host=ssh_host, remote_cmd=remote_cmd)
        state = await _it._fetch_instance_state(entry)
        remote_pid = state.get("pid", 0)
        if remote_pid and hasattr(ssh_backend, "stop_remote"):
            try:
                await ssh_backend.stop_remote(remote_pid)
            except Exception as e:
                logger.warning(f"Error stopping remote process for {instance_id}: {e}")

    return ToolResult(
        data={"id": instance_id, "stopped": True},
        broadcast=[
            {"type": "instances_changed", "instances": _it._instance_list_sync()}
        ],
    )


@register_tool("stop_instance")
async def stop_instance(instance_id: str) -> dict:
    """Stop a tracked Fantastic instance.

    Sends SIGTERM, waits 3s, then SIGKILL if still alive.

    Args:
        instance_id: The instance ID (from launch_instance or list_instances).
    """
    tr = await _stop_instance(instance_id)
    await _fire_broadcasts(tr)
    return tr.data


@register_dispatch("list_instances")
async def _list_instances() -> ToolResult:
    return ToolResult(data=await _it._instance_list(check_http=True))


@register_tool("list_instances")
async def list_instances() -> list[dict]:
    """List all tracked Fantastic instances with their status."""
    tr = await _list_instances()
    return tr.data


@register_dispatch("register_instance")
async def _register_instance(
    url: str,
    project_dir: str = "",
    name: str = "",
) -> ToolResult:
    if not project_dir:
        return ToolResult(
            data={"error": "project_dir is required for register_instance"}
        )

    own_port = _it._get_own_port()
    from urllib.parse import urlparse

    if own_port:
        p = urlparse(url)
        h = p.hostname or ""
        if h in {"127.0.0.1", "localhost", "0.0.0.0", "::1"} and p.port == own_port:
            return ToolResult(data={"error": "Cannot register self"})

    tracked_entry: dict[str, Any] = {
        "project_dir": project_dir,
        "name": name or os.path.basename(project_dir),
        "backend": "local",
    }
    _it._add_tracked(tracked_entry)
    iid = _it._instance_id(project_dir)

    return ToolResult(
        data={"id": iid, "project_dir": project_dir, "name": tracked_entry["name"]},
        broadcast=[
            {"type": "instances_changed", "instances": _it._instance_list_sync()}
        ],
    )


@register_tool("register_instance")
async def register_instance(
    url: str,
    project_dir: str = "",
    name: str = "",
) -> dict:
    """Register a Fantastic instance for discovery.

    Any instance can register any other instance. Both sides can track each other.

    Args:
        url: The instance's base URL (e.g. "http://127.0.0.1:49200").
        project_dir: The instance's project directory path.
        name: Human-readable name for the instance.
    """
    tr = await _register_instance(url, project_dir, name)
    await _fire_broadcasts(tr)
    return tr.data


@register_dispatch("unregister_instance")
async def _unregister_instance(instance_id: str) -> ToolResult:
    if not _it._remove_tracked(instance_id):
        return ToolResult(data={"error": f"Instance {instance_id} not found"})
    _it._launched_processes.pop(instance_id, None)
    return ToolResult(
        data={"id": instance_id, "unregistered": True},
        broadcast=[
            {"type": "instances_changed", "instances": _it._instance_list_sync()}
        ],
    )


@register_tool("unregister_instance")
async def unregister_instance(instance_id: str) -> str:
    """Remove a Fantastic instance from the discovery registry.

    Args:
        instance_id: The instance ID to remove.
    """
    tr = await _unregister_instance(instance_id)
    if "error" in tr.data:
        return tr.data
    await _fire_broadcasts(tr)
    return tr.data


@register_dispatch("restart_instance")
async def _restart_instance(instance_id: str) -> ToolResult:
    entry = _it._find_tracked(instance_id)
    if entry is None:
        return ToolResult(data={"error": f"Instance {instance_id} not found"})

    project_dir = entry["project_dir"]
    ssh_host = entry.get("ssh_host", "")

    # Stop old process
    await _stop_instance(instance_id)

    # Relaunch with original remote_cmd
    remote_cmd = entry.get("remote_cmd", "fantastic")
    tr = await _launch_instance(project_dir, ssh_host=ssh_host, remote_cmd=remote_cmd)
    if "error" in tr.data:
        return tr

    tr.data["restarted_from"] = instance_id
    return tr


@register_tool("restart_instance")
async def restart_instance(instance_id: str) -> dict:
    """Restart a Fantastic instance by its registry ID.

    Looks up project_dir from the registry, stops the old process if still
    alive, and launches a fresh instance on a new port.

    Args:
        instance_id: The instance ID from the registry.
    """
    tr = await _restart_instance(instance_id)
    await _fire_broadcasts(tr)
    return tr.data


@register_dispatch("list_registered_instances")
async def _list_registered_instances() -> ToolResult:
    """Alias — merged into list_instances."""
    return await _list_instances()


@register_tool("list_registered_instances")
async def list_registered_instances() -> list[dict]:
    """List all registered Fantastic instances (both locally launched and remote)."""
    tr = await _list_registered_instances()
    return tr.data
