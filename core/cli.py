"""
CLI entry point for Fantastic.

Progressive architecture: Core -> Server -> AI

Usage:
  fantastic                            # adaptive: Core-only, Core+Server, or connect
  fantastic --port 8000                # custom port
  fantastic add <bundle>               # add a bundle (creates agent)
  fantastic add <bundle> --name debug  # add a named instance
  fantastic remove <bundle>            # remove a bundle (cascade delete)
  fantastic remove <bundle> --name x   # remove a specific instance
  fantastic list                       # show bundles + instances tree
  fantastic --requirements requirements.txt  # install packages on start
"""

import argparse
import asyncio
import importlib.util
import inspect
import json
import logging
import os
import signal
import socket
from pathlib import Path

import uvicorn

# ─── Banner ────────────────────────────────────────────────────

_NM = "\033[38;5;165m"  # neon magenta
_BM = "\033[95m"  # bright magenta
_B = "\033[1m"  # bold
_D = "\033[2m"  # dim
_C = "\033[36m"  # cyan
_R = "\033[0m"  # reset


_G = "\033[32m"  # green
_Y = "\033[33m"  # yellow


def _banner(project_dir: str = "", status: dict | None = None):
    """Print the Fantastic banner — Diia-style asymmetric + system status."""
    print()
    print(f"  {_NM}█{_R}")
    print(f"  {_NM}█{_R}  {_BM}{_B}FANTASTIC{_R}")
    print(f"  {_NM}█{_R}")
    if project_dir:
        print(f"     {_D}{project_dir}{_R}")
    if status:
        parts = []
        for key, val in status.items():
            if val is True:
                parts.append(f"{_G}{key}{_R}")
            elif val is False:
                parts.append(f"{_D}{key}{_R}")
            else:
                parts.append(f"{_G}{key}{_R} {_D}{val}{_R}")
        print(f"     {' · '.join(parts)}")
    print()


def _find_free_port(host: str, start: int = 8888, max_tries: int = 100) -> int:
    """Find the first available port starting from `start`."""
    for port in range(start, start + max_tries):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            try:
                s.bind((host, port))
                return port
            except OSError:
                continue
    raise RuntimeError(f"No free port found in range {start}-{start + max_tries - 1}")


def _port_available(host: str, port: int) -> bool:
    """Check if a port is available for binding."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            s.bind((host, port))
            return True
        except OSError:
            return False


def _pid_alive(pid: int) -> bool:
    """Check if a process with the given PID is alive."""
    try:
        os.kill(pid, 0)
        return True
    except (OSError, ProcessLookupError):
        return False


def _read_saved_config(project_dir: str) -> dict:
    """Read config from .fantastic/config.json."""
    config_path = Path(project_dir) / ".fantastic" / "config.json"
    if config_path.exists():
        try:
            return json.loads(config_path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError):
            pass
    return {}


def _write_saved_config(project_dir: str, config: dict) -> None:
    """Write config to .fantastic/config.json."""
    config_dir = Path(project_dir) / ".fantastic"
    config_dir.mkdir(parents=True, exist_ok=True)
    (config_dir / "config.json").write_text(
        json.dumps(config, indent=2),
        encoding="utf-8",
    )


def _call_bundle_hook(
    bundle_dir: Path, hook_name: str, project_dir: str, **kwargs
) -> None:
    """Call a hook function on a bundle's tools.py if it exists."""
    tools_file = bundle_dir / "tools.py"
    if not tools_file.exists():
        return
    try:
        spec = importlib.util.spec_from_file_location(
            f"bundle_{bundle_dir.name}_hook",
            tools_file,
        )
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        fn = getattr(mod, hook_name, None)
        if fn:
            sig = inspect.signature(fn)
            valid_kwargs = {k: v for k, v in kwargs.items() if k in sig.parameters}
            fn(project_dir, **valid_kwargs)
    except Exception as e:
        print(f"  Warning: hook {hook_name} failed for {bundle_dir.name}: {e}")


def _has_agents(project_dir: str) -> bool:
    """Check if .fantastic/agents/ has any agent directories."""
    agents_dir = Path(project_dir) / ".fantastic" / "agents"
    if not agents_dir.exists():
        return False
    return any(d.is_dir() and (d / "agent.json").exists() for d in agents_dir.iterdir())


# ─── Subcommands ───────────────────────────────────────────────


def _cmd_add(args):
    """Add a bundle: create its agent."""
    from ._paths import bundled_agents_dir
    from .bundles import BundleStore

    project_dir = os.path.abspath(args.project_dir)
    bundle_name = args.bundle
    name = getattr(args, "name", "") or ""
    working_dir = getattr(args, "working_dir", "") or ""
    from_source = getattr(args, "from_source", "") or ""

    if from_source:
        # External plugin: install (copy files only, no agent creation)
        from ._install import install_plugin

        try:
            install_plugin(Path(project_dir), from_source, bundle_name)
        except RuntimeError as e:
            print(f"  Error: {e}")
            return
        print(f"  Installed plugin: {bundle_name}")
        return

    # If server is running, route through REST API so agents appear instantly
    config = _read_saved_config(project_dir)
    saved_pid = config.get("pid", 0)
    saved_port = config.get("port", 0)
    if saved_pid and _pid_alive(saved_pid) and saved_port:
        import httpx

        try:
            resp = httpx.post(
                f"http://localhost:{saved_port}/api/call",
                json={
                    "tool": "add_bundle",
                    "args": {
                        "bundle_name": bundle_name,
                        "name": name,
                        "working_dir": working_dir,
                    },
                },
                timeout=30,
            )
            data = resp.json()
            if "error" in (data.get("result") or {}):
                print(f"  Error: {data['result']['error']}")
                return
        except Exception as e:
            print(f"  Server call failed: {e}, falling back to offline add")
        else:
            print(f"  Added: {bundle_name}" + (f" ({name})" if name else ""))
            return

    # Offline: resolve bundle dir and call on_add directly
    store = BundleStore(bundled_agents_dir())
    bundle = store.get_bundle(bundle_name)
    if bundle:
        bundle_dir = bundled_agents_dir() / bundle_name
    else:
        from ._install import get_plugin_dir

        plugin_dir = get_plugin_dir(Path(project_dir), bundle_name)
        if plugin_dir.exists() and (plugin_dir / "template.json").exists():
            bundle_dir = plugin_dir
        else:
            available = [
                b.get("bundle", b.get("name", "?")) for b in store.list_bundles()
            ]
            print(f"  Unknown bundle: {bundle_name}")
            if available:
                print(f"  Available: {', '.join(available)}")
            return

    _call_bundle_hook(
        bundle_dir, "on_add", project_dir, name=name, working_dir=working_dir
    )

    print(f"  Added: {bundle_name}" + (f" ({name})" if name else ""))


def _cmd_remove(args):
    """Remove a bundle instance (cascade delete)."""
    from .agent_store import AgentStore

    project_dir = os.path.abspath(args.project_dir)
    bundle_name = args.bundle
    name = getattr(args, "name", "") or ""

    store = AgentStore(Path(project_dir))
    store.init()
    agents = store.list_agents()
    bundle_agents = [a for a in agents if a.get("bundle") == bundle_name]

    if not bundle_agents:
        print(f"  No {bundle_name} instances found")
        return

    # Name required if multiple instances
    if not name and len(bundle_agents) > 1:
        names = [a.get("display_name", a["id"]) for a in bundle_agents]
        print(f"  Specify --name. Active: {', '.join(names)}")
        return

    # Filter by name
    if name:
        targets = [a for a in bundle_agents if a.get("display_name") == name]
        if not targets:
            names = [a.get("display_name", a["id"]) for a in bundle_agents]
            print(f"  No {bundle_name} '{name}' found. Active: {', '.join(names)}")
            return
    else:
        targets = bundle_agents

    for agent in targets:
        # Cascade: delete children first
        children = store.list_children(agent["id"])
        for child in children:
            store.delete_agent(child["id"])
        store.delete_agent(agent["id"])

    print(f"  Removed: {bundle_name}" + (f" ({name})" if name else ""))


def _cmd_list(args):
    """List bundles as a tree with instances."""
    from ._paths import bundled_agents_dir
    from .bundles import BundleStore
    from .agent_store import AgentStore
    from ._install import list_plugin_dirs

    project_dir = os.path.abspath(args.project_dir)
    store = BundleStore(bundled_agents_dir())
    bundles = store.list_bundles()

    agent_store = AgentStore(Path(project_dir))
    agent_store.init()
    agents = agent_store.list_agents()

    # Group agents by bundle
    bundle_instances: dict[str, list[dict]] = {}
    for a in agents:
        b = a.get("bundle")
        if b:
            bundle_instances.setdefault(b, []).append(a)

    print()
    print(f"  {_B}Bundles{_R}")
    seen = set()
    for b in bundles:
        bname = b.get("bundle", b.get("name", "?"))
        seen.add(bname)
        instances = bundle_instances.get(bname, [])
        if instances:
            for inst in instances:
                display = inst.get("display_name") or inst["id"]
                children = agent_store.list_children(inst["id"])
                print(
                    f"  {_G}●{_R} {bname}  {display}  ({inst['id']})  [{len(children)} agents]"
                )
        else:
            print(f"  {_D}○ {bname}{_R}  [available]")

    # Installed plugins
    for pdir in list_plugin_dirs(Path(project_dir)):
        try:
            tmpl = json.loads((pdir / "template.json").read_text())
            pname = tmpl.get("bundle", pdir.name)
        except Exception:
            pname = pdir.name
        if pname in seen:
            continue
        seen.add(pname)
        instances = bundle_instances.get(pname, [])
        if instances:
            for inst in instances:
                display = inst.get("display_name") or inst["id"]
                children = agent_store.list_children(inst["id"])
                print(
                    f"  {_G}●{_R} {pname}  {display}  ({inst['id']})  [{len(children)} agents]  {_D}[plugin]{_R}"
                )
        else:
            print(f"  {_D}○ {pname}{_R}  [plugin]")

    if not bundles and not list_plugin_dirs(Path(project_dir)):
        print("  (no bundles found)")
    print()


# ─── Log → conversation routing ────────────────────────────────


class _ConversationLogHandler(logging.Handler):
    """Route log records through conversation.format_entry for aligned output."""

    # Map verbose logger names to short display names
    _SHORT = {
        "_plugin_loader": "plugins",
        "_lifespan": "server",
        "error": "server",
        "access": "server",
    }

    def emit(self, record):
        from . import conversation

        msg = record.getMessage()
        raw = record.name.split(".")[-1]
        who = self._SHORT.get(raw, raw)
        # Extract actor from message prefix "actor msg"
        if who in ("server",) and " " in msg:
            first_word = msg.split()[0]
            if first_word in ("Server", "Fantastic"):
                who = "fantastic"
            elif first_word.isalpha() and len(first_word) < 20:
                who = first_word
                msg = msg[len(first_word) + 1 :]
        entry = conversation.say(who, msg)
        print(conversation.format_entry(entry))


def _install_conversation_log_handler():
    """Replace default handlers on core + uvicorn loggers with conversation routing."""
    handler = _ConversationLogHandler()
    for name in ("core", "uvicorn", "uvicorn.error", "uvicorn.access"):
        lg = logging.getLogger(name)
        lg.handlers.clear()
        lg.addHandler(handler)
        lg.propagate = False


# ─── Adaptive start ────────────────────────────────────────────


async def _run_with_server(args, project_dir, port, auto_run_bundle: str | None = None):
    """Core + Server: server and input loop as concurrent tasks."""
    from .input_loop import InputLoop

    # Route server logs through conversation buffer (padded + aligned)
    _install_conversation_log_handler()

    os.environ["PROJECT_DIR"] = project_dir
    os.environ["SERVER_PORT"] = str(port)

    if args.requirements:
        os.environ["REQUIREMENTS_FILE"] = os.path.abspath(args.requirements)

    cli_cmd = " ".join(args.cli) if args.cli else None
    if cli_cmd:
        os.environ["FANTASTIC_CLI"] = cli_cmd

    config = uvicorn.Config(
        "core.server:app",
        host=args.host,
        port=port,
        log_level="warning",
    )
    server = uvicorn.Server(config)
    loop = InputLoop()

    # Ensure server shuts down on SIGTERM/SIGINT (process kill, Ctrl+C)
    aloop = asyncio.get_event_loop()
    for sig in (signal.SIGTERM, signal.SIGINT):
        aloop.add_signal_handler(sig, lambda: _shutdown_server(server))

    server_task = asyncio.create_task(server.serve())
    # Small delay so server starts before input loop prints
    await asyncio.sleep(0.5)
    if auto_run_bundle:
        await loop._run_chat_agent(auto_run_bundle)
    await loop.run()
    server.should_exit = True
    await server_task


def _shutdown_server(server):
    """Signal handler: tell uvicorn to exit so lifespan cleanup runs."""
    server.should_exit = True


def _cmd_start(args):
    """Adaptive start: Core-only, Core+Server, or connect to running."""
    project_dir = os.path.abspath(args.project_dir)
    saved = _read_saved_config(project_dir)

    # Check if server is already running (singleton per folder)
    saved_pid = saved.get("pid")
    if saved_pid and _pid_alive(saved_pid):
        saved_port = saved.get("port", 8888)
        url = f"http://{args.host}:{saved_port}"
        _banner(
            project_dir=project_dir,
            status={
                "core": True,
                "server": True,
                "ai": False,
            },
        )
        from . import conversation

        entry = conversation.say("fantastic", f"server already running at {url}")
        print(conversation.format_entry(entry))
        print()
        from .input_loop import InputLoop

        loop = InputLoop()
        asyncio.run(loop.run())
        return

    # Always start server when port is free
    # Port resolution: 1) --port flag  2) saved config  3) default 8888
    if args.port is not None:
        port = args.port
    elif saved.get("port"):
        port = saved["port"]
    else:
        port = 8888

    # Retry binding 3 times (handles TIME_WAIT on restart), then find new port
    import time

    for attempt in range(3):
        if _port_available(args.host, port):
            break
        time.sleep(0.3)
    else:
        port = _find_free_port(args.host)

    url = f"http://{args.host}:{port}"
    _banner(
        project_dir=project_dir,
        status={
            "core": True,
            "server": True,
            "ai": False,
        },
    )

    # Fresh project? Auto-add + run quickstart wizard
    bundle_name = None
    if not _has_agents(project_dir):
        from ._paths import bundled_agents_dir

        _call_bundle_hook(bundled_agents_dir() / "quickstart", "on_add", project_dir)
        bundle_name = "quickstart"

    asyncio.run(_run_with_server(args, project_dir, port, auto_run_bundle=bundle_name))


def _cmd_serve(args):
    """Start the server directly (headless, no input loop)."""
    project_dir = os.path.abspath(args.project_dir)
    saved = _read_saved_config(project_dir)

    # Check if server is already running (saved PID is alive)
    saved_pid = saved.get("pid")
    if saved_pid and _pid_alive(saved_pid):
        saved_port = saved.get("port", 8888)
        url = f"http://{args.host}:{saved_port}"
        _banner(
            project_dir=project_dir,
            status={
                "core": True,
                "server": True,
                "ai": False,
            },
        )
        print(f"  {_D}Already running at {url}{_R}")
        return

    # Port resolution: 1) --port flag  2) saved config  3) default 8888
    if args.port is not None:
        port = args.port
    elif saved.get("port"):
        port = saved["port"]
    else:
        port = 8888

    # Retry binding 3 times (handles TIME_WAIT on restart), then find new port
    import time

    for attempt in range(3):
        if _port_available(args.host, port):
            break
        time.sleep(0.3)
    else:
        port = _find_free_port(args.host)

    # Set project directory and server port
    os.environ["PROJECT_DIR"] = project_dir
    os.environ["SERVER_PORT"] = str(port)

    if args.requirements:
        os.environ["REQUIREMENTS_FILE"] = os.path.abspath(args.requirements)

    cli_cmd = " ".join(args.cli) if args.cli else None
    if cli_cmd:
        os.environ["FANTASTIC_CLI"] = cli_cmd

    url = f"http://{args.host}:{port}"
    _banner(
        project_dir=project_dir,
        status={
            "core": True,
            "server": True,
            "ai": False,
        },
    )

    uvicorn.run(
        "core.server:app",
        host=args.host,
        port=port,
        reload=False,
        log_level=args.log_level,
    )


async def _run_with_server_and_bundle(args, project_dir):
    """Start server, run the chat agent, then continue input loop."""
    from .input_loop import InputLoop

    _install_conversation_log_handler()
    os.environ["PROJECT_DIR"] = project_dir

    port = args.port or _read_saved_config(project_dir).get("port", 8888)
    import time

    for _ in range(3):
        if _port_available(args.host, port):
            break
        time.sleep(0.3)
    else:
        port = _find_free_port(args.host)

    os.environ["SERVER_PORT"] = str(port)
    config = uvicorn.Config(
        "core.server:app", host=args.host, port=port, log_level="warning"
    )
    server = uvicorn.Server(config)
    loop = InputLoop()

    server_task = asyncio.create_task(server.serve())
    await asyncio.sleep(0.5)
    await loop._run_chat_agent(args.bundle)
    await loop.run()
    server.should_exit = True
    await server_task


def _cmd_run(args):
    """Run a bundle's @chat_run hook, starting engine if needed."""
    project_dir = os.path.abspath(args.project_dir)
    _banner(project_dir=project_dir, status={"core": True, "server": True, "ai": False})
    asyncio.run(_run_with_server_and_bundle(args, project_dir))


def main():
    parser = argparse.ArgumentParser(description="Fantastic")
    parser.add_argument(
        "--project-dir",
        type=str,
        default=os.getcwd(),
        help="Project directory (default: CWD)",
    )
    subs = parser.add_subparsers(dest="command")

    # ─── add <bundle> ──────────────────────────────────────────
    p_add = subs.add_parser("add", help="Add a bundle")
    p_add.add_argument("bundle", help="Bundle name")
    p_add.add_argument("--name", default="", help="Instance name (e.g. --name main)")
    p_add.add_argument(
        "--working-dir", default="", help="Working directory for this instance"
    )
    p_add.add_argument(
        "--from",
        dest="from_source",
        default="",
        help="Git URL or local path to external plugin",
    )

    # ─── remove <bundle> ───────────────────────────────────────
    p_remove = subs.add_parser("remove", help="Remove a bundle")
    p_remove.add_argument("bundle", help="Bundle name")
    p_remove.add_argument("--name", default="", help="Instance name to remove")

    # ─── list ──────────────────────────────────────────────────
    subs.add_parser("list", help="List available bundles")

    # ─── serve (headless, no input loop) ───────────────────────
    subs.add_parser("serve", help="Start server without input loop")

    # ─── run <bundle> ─────────────────────────────────────────
    p_run = subs.add_parser("run", help="Run a bundle's @chat_run agent")
    p_run.add_argument("bundle", help="Bundle name (e.g. quickstart)")

    # ─── Shared flags ──────────────────────────────────────────
    parser.add_argument(
        "--port", type=int, default=None, help="Server port (default: auto from 8888)"
    )
    parser.add_argument(
        "--host", default="127.0.0.1", help="Server host (default: 127.0.0.1)"
    )
    parser.add_argument(
        "--requirements",
        type=str,
        help="Path to requirements.txt to install on startup",
    )
    parser.add_argument(
        "--cli",
        nargs=argparse.REMAINDER,
        default=None,
        help="Auto-launch an agent with this command (e.g. --cli claude --model sonnet)",
    )
    parser.add_argument(
        "--log-level", default="info", choices=["debug", "info", "warning", "error"]
    )

    args = parser.parse_args()

    logging.basicConfig(
        level=getattr(logging, args.log_level.upper(), logging.INFO),
        format="%(asctime)s [%(name)s] %(levelname)s: %(message)s",
    )

    if args.command == "add":
        _cmd_add(args)
    elif args.command == "remove":
        _cmd_remove(args)
    elif args.command == "list":
        _cmd_list(args)
    elif args.command == "serve":
        _cmd_serve(args)
    elif args.command == "run":
        _cmd_run(args)
    else:
        _cmd_start(args)


if __name__ == "__main__":
    main()
