"""Lifespan context manager — startup/shutdown, instance monitor, file watcher."""

import asyncio
import json
import logging
import os
import subprocess
import sys
from contextlib import asynccontextmanager
from pathlib import Path

from fastapi import FastAPI

from ..engine import Engine
from ..tools import init_tools, install_log_buffer, _instance_list_sync, _load_tracked, _pid_alive, _launched_processes
from ..process_runner import ProcessRunner
from ..agent import discover_autorun
from .._paths import fantastic_md_path
from . import _state

logger = logging.getLogger(__name__)


async def watch_project_dir():
    """Watch project directory for file changes and broadcast to WS clients."""
    try:
        import watchfiles
        from . import broadcast

        def _watch_filter(change, path: str) -> bool:
            for skip in ("/.fantastic/", "/.git/", "/node_modules/", "/__pycache__/"):
                if skip in path:
                    return False
            return True

        async for _changes in watchfiles.awatch(str(_state.engine.project_dir), watch_filter=_watch_filter):
            await broadcast({"type": "files_changed", "files": _state.engine.list_files()})
    except asyncio.CancelledError:
        pass
    except Exception as e:
        logger.error(f"File watcher error: {e}")


@asynccontextmanager
async def lifespan(app: FastAPI):
    from . import broadcast, mount_all_apps

    # Install server log buffer early so all startup messages are captured
    install_log_buffer()

    project_dir = os.getenv("PROJECT_DIR")

    # Auto-install requirements if specified
    requirements_file = os.getenv("REQUIREMENTS_FILE")
    if requirements_file and Path(requirements_file).exists():
        logger.info(f"Installing requirements from {requirements_file}")
        import shutil
        if shutil.which("uv"):
            subprocess.run(["uv", "pip", "install", "-r", requirements_file], check=True)
        else:
            subprocess.run(
                [sys.executable, "-m", "pip", "install", "-r", requirements_file],
                check=True,
            )

    _state.engine = Engine(
        project_dir=project_dir,
        broadcast=broadcast,
    )
    await _state.engine.start()

    # Process runner — generic PTY manager, no plugin-specific knowledge
    _state.process_runner = ProcessRunner(
        on_output=_generic_process_output,
        agents_dir=_state.engine.project_dir / ".fantastic" / "agents",
    )

    # Initialize tools — bundles register hooks here
    init_tools(_state.engine, broadcast, _state.process_runner)

    # Determine server port
    server_port = os.getenv("SERVER_PORT", "")
    if not server_port:
        for i, arg in enumerate(sys.argv):
            if arg == "--port" and i + 1 < len(sys.argv):
                server_port = sys.argv[i + 1]
                break
        if not server_port:
            server_port = "8888"

    # Mount plugin routes
    mount_all_apps()

    # Run plugin lifespan hooks
    for hook in _state._lifespan_hooks:
        await hook(_state, broadcast)
    _state._lifespan_hooks_ran = len(_state._lifespan_hooks)

    # Template-render fantastic.md into .fantastic/ (re-render every startup for correct port)
    fantastic_dest = _state.engine.project_dir / ".fantastic" / "fantastic.md"
    src = fantastic_md_path()
    if src.exists():
        template = src.read_text(encoding="utf-8")
        rendered = template.replace("{{SERVER_URL}}", f"http://localhost:{server_port}")
        fantastic_dest.write_text(rendered, encoding="utf-8")

    # Persist PID + port so CLI can detect running server and reuse port
    config = _state.engine.store.get_config()
    config["pid"] = os.getpid()
    config["port"] = int(server_port)
    _state.engine.store.set_config(config)

    _state.file_watcher_task = None

    # Auto-launch agent with --cli command
    cli_cmd = os.getenv("FANTASTIC_CLI")
    if cli_cmd:
        cli_bundle = os.getenv("FANTASTIC_CLI_BUNDLE", "")
        agent = _state.engine.create_agent(bundle=cli_bundle or None)
        agent_id = agent["id"]

        await _state.process_runner.create(
            agent_id,
            cols=120, rows=40,
            cwd=str(_state.engine.resolve_working_dir(agent_id)),
            welcome_command=cli_cmd,
        )
        _state.engine.update_agent_meta(agent_id, process_params={
            "command": None,
            "args": None,
            "welcome_command": cli_cmd,
        })
        await broadcast({"type": "agent_created", "agent": agent})
        logger.info(f"Auto-launched agent {agent_id} with: {cli_cmd}")

    # Auto-start processes via @autorun discovery from previous session
    agents = _state.engine.store.list_agents()
    agents_dir = _state.engine.project_dir / ".fantastic" / "agents"
    for agent in agents:
        aid = agent["id"]
        if _state.process_runner.exists(aid):
            continue

        source_path = agents_dir / aid / "source.py"
        autorun_config = discover_autorun(source_path)

        if autorun_config and autorun_config.get("pty"):
            params = agent.get("process_params") or {}
            await _state.process_runner.create(
                aid,
                cols=80, rows=24,
                cwd=str(_state.engine.resolve_working_dir(aid)),
                command=params.get("command"),
                args=params.get("args"),
                welcome_command=params.get("welcome_command"),
            )
            logger.info(f"Auto-started process {aid}")

    # Start scrollback flush loop
    await _state.process_runner.start_flush_loop()

    # Background loop: detect instance status changes and notify frontend
    _prev_statuses: dict[str, str] = {}

    async def _instance_monitor():
        nonlocal _prev_statuses
        loop = asyncio.get_event_loop()
        while True:
            await asyncio.sleep(5)
            try:
                instances = await loop.run_in_executor(None, _instance_list_sync)
                cur = {i["id"]: i["status"] for i in instances}
                if cur != _prev_statuses:
                    _prev_statuses = cur
                    await broadcast({"type": "instances_changed", "instances": instances})
            except Exception:
                pass

    instance_monitor_task = asyncio.create_task(_instance_monitor())

    logger.info(f"Server started on port {server_port}")
    yield
    instance_monitor_task.cancel()
    try:
        await instance_monitor_task
    except asyncio.CancelledError:
        pass
    if _state.file_watcher_task:
        _state.file_watcher_task.cancel()
        try:
            await _state.file_watcher_task
        except asyncio.CancelledError:
            pass
    await _state.process_runner.close_all()
    # Stop launched instances on shutdown
    import signal as _signal
    for proc in list(_launched_processes.values()):
        if proc.returncode is None:
            try:
                proc.terminate()
            except Exception:
                pass
    _launched_processes.clear()
    for entry in _load_tracked():
        if entry.get("ssh_host"):
            tunnel_pid = entry.get("tunnel_pid", 0)
            if tunnel_pid and _pid_alive(tunnel_pid):
                try:
                    os.kill(tunnel_pid, _signal.SIGTERM)
                except (ProcessLookupError, PermissionError, OSError):
                    pass
        else:
            config_path = Path(entry["project_dir"]) / ".fantastic" / "config.json"
            try:
                if config_path.exists():
                    cfg = json.loads(config_path.read_text())
                    pid = cfg.get("pid", 0)
                    if pid and _pid_alive(pid):
                        os.killpg(os.getpgid(pid), _signal.SIGTERM)
            except (ProcessLookupError, PermissionError, OSError, Exception):
                pass
    # Clear PID from config on clean shutdown
    try:
        config = _state.engine.store.get_config()
        config.pop("pid", None)
        _state.engine.store.set_config(config)
    except Exception:
        pass
    await _state.engine.stop()
    logger.info("Fantastic server stopped")


async def _generic_process_output(agent_id: str, data: str):
    """Generic PTY output handler — broadcasts process_output."""
    if data:
        from . import broadcast
        await broadcast({"type": "process_output", "agent_id": agent_id, "data": data})
