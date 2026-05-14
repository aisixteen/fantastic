"""Substrate-level CLI dispatch.

`dispatch_argv(kernel, argv)` is the only entrypoint. Two shapes:

  ONE-SHOT (boots, dispatches, exits):
    fantastic install <project_dir> [pkg ...]   # uv venv + pip install
    fantastic install-bundle <spec>             # uv pip install a bundle
    fantastic reflect [<id>]                    # sugar: <id> reflect
    fantastic <id> <verb> [k=v ...]             # generic RPC

  DEFAULT — LONG-RUNNING:
    fantastic                                    # boot all + REPL (tty) + block while web alive
    fantastic                       (no tty)     # boot all; block iff web exists, else exit

Web composition is **explicit** — there's no `--port` flag. To run a
daemon, first persist a web agent record:

    fantastic core create_agent handler_module=web.tools port=8888

Then `fantastic` rehydrates it and blocks (because uvicorn is alive).
The kernel exits when nothing's keeping it alive:
  - REPL: exits on EOF / `exit` / `quit`
  - web: blocks the kernel via _block_forever() while present
  - neither composed → exit immediately
"""

from __future__ import annotations

import asyncio
import json
import shlex
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

from kernel._bundles import _find_bundle_module
from kernel._lock import FantasticLock, _pid_alive, _read_lock


# ─── public entry ────────────────────────────────────────────────


async def dispatch_argv(kernel, argv: list[str]) -> Any:
    """argv → action. One-shot subcommands routed by first word;
    everything else falls into the default long-running mode."""
    if argv and argv[0] in ("-h", "--help", "help"):
        _print_help()
        return None

    # One-shot subcommands.
    if argv:
        cmd = argv[0]
        if cmd == "install":
            return install(argv[1:])
        if cmd == "install-bundle":
            return install_bundle(argv[1:])
        if cmd == "reflect":
            return await reflect(kernel, argv[1:])
        # Generic <id> <verb> [k=v ...].
        if len(argv) >= 2:
            return await call(kernel, cmd, argv[1:])
        # Single positional that isn't a subcommand and has no verb.
        print(
            "usage: fantastic <id> <verb> [k=v ...]\n"
            "       fantastic reflect [<id>]\n"
            "       fantastic install <project_dir> [pkg ...]\n"
            "       fantastic install-bundle <spec> [--into <project>]\n"
            "       fantastic                              (interactive REPL when stdin is a tty;\n"
            "                                               daemon when a web agent is persisted)",
            file=sys.stderr,
        )
        sys.exit(2)

    # Default long-running mode.
    return await _default(kernel)


# ─── one-shot modes ──────────────────────────────────────────────


async def call(kernel, target: str, rest: list[str]) -> None:
    """One-shot RPC. Always acquires the PID lock + dispatches in-
    process. Fails if another fantastic owns the dir (use kernel_bridge
    to forward to it over WS instead).

    The kernel does NOT speak HTTP for substrate calls. Browsers /
    cross-kernel callers go through the WS frame protocol exposed by
    `web` (and reused by `kernel_bridge` for in-process bridges)."""
    if not rest:
        print(
            "usage: fantastic <target_id> <verb> [k=v ...]",
            file=sys.stderr,
        )
        sys.exit(2)
    verb, *kv_args = rest
    body = {"type": verb, **_parse_kv(kv_args)}
    cur = _read_lock()
    if cur and isinstance(cur.get("pid"), int) and _pid_alive(cur["pid"]):
        print(
            f"[call] another fantastic owns this dir (pid={cur.get('pid')}). "
            "Stop it, or forward your call over the WS bridge "
            "(see kernel_bridge bundle).",
            file=sys.stderr,
        )
        sys.exit(1)
    with FantasticLock():
        reply = await kernel.send(target, body)
        print(json.dumps(reply, indent=2, default=str))
    return None


async def reflect(kernel, rest: list[str]) -> None:
    """Sugar for `<target> reflect [k=v ...]`. Default target:
    'kernel'. The first token is the target unless it's a `k=v` pair,
    so `fantastic reflect return_readme=true` reflects the kernel with
    the flag, and `fantastic reflect <id> return_readme=true` reflects
    that agent.

    Read-only — dispatched in-process WITHOUT the PID lock, so it
    works whether or not a daemon owns the dir. A one-shot kernel sees
    disk-backed records (the agent tree, readmes); live process-memory
    state belongs to the daemon and isn't reflected here."""
    if rest and "=" not in rest[0]:
        target, kv = rest[0], rest[1:]
    else:
        target, kv = "kernel", rest
    body = {"type": "reflect", **_parse_kv(kv)}
    reply = await kernel.send(target, body)
    print(json.dumps(reply, indent=2, default=str))
    return None


def install(rest: list[str]) -> None:
    """uv venv + uv pip install + point python_runtime records at .venv."""
    if not rest:
        print(
            "usage: fantastic install <project_dir> [pkg ...]",
            file=sys.stderr,
        )
        sys.exit(2)
    project_dir = rest[0]
    packages = list(rest[1:])
    proj = Path(project_dir).expanduser().resolve()
    if not proj.is_dir():
        print(f"[install] not a directory: {proj}", file=sys.stderr)
        sys.exit(2)
    if shutil.which("uv") is None:
        print("[install] uv not on PATH; install uv first", file=sys.stderr)
        sys.exit(2)

    venv_dir = proj / ".venv"
    print(f"[install] uv venv {venv_dir}", file=sys.stderr)
    r = subprocess.run(
        ["uv", "venv", str(venv_dir)],
        cwd=proj,
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        print(r.stderr, file=sys.stderr)
        sys.exit(r.returncode)
    print(r.stdout.strip() or "  venv created", file=sys.stderr)

    if packages:
        cmd = [
            "uv",
            "pip",
            "install",
            "--python",
            str(venv_dir / "bin" / "python"),
            *packages,
        ]
        print(f"[install] {' '.join(cmd)}", file=sys.stderr)
        r = subprocess.run(cmd, cwd=proj, capture_output=True, text=True)
        if r.returncode != 0:
            print(r.stderr, file=sys.stderr)
            sys.exit(r.returncode)
        print(r.stdout.strip() or "  packages installed", file=sys.stderr)

    agents_dir = proj / ".fantastic" / "agents"
    updated = 0
    if agents_dir.is_dir():
        for entry in sorted(agents_dir.iterdir()):
            agent_json = entry / "agent.json"
            if not agent_json.exists():
                continue
            try:
                rec = json.loads(agent_json.read_text())
            except (json.JSONDecodeError, OSError):
                continue
            if rec.get("handler_module") != "python_runtime.tools":
                continue
            if rec.get("venv") == ".venv":
                continue
            rec["venv"] = ".venv"
            agent_json.write_text(json.dumps(rec, indent=2))
            updated += 1
            print(f"  updated {rec['id']} → venv=.venv", file=sys.stderr)
    print(
        f"[install] done — venv={venv_dir} | python_runtime records updated: {updated}",
        file=sys.stderr,
    )


def install_bundle(rest: list[str]) -> None:
    """uv pip install a fantastic bundle into kernel venv or <proj>/.venv."""
    if not rest:
        print(
            "usage: fantastic install-bundle <spec> [--into <project>]\n"
            "  spec is a uv pip install argument: a git URL, "
            "a PyPI name, or a local path.",
            file=sys.stderr,
        )
        sys.exit(2)
    spec = rest[0]
    into: str | None = None
    args = list(rest[1:])
    if "--into" in args:
        i = args.index("--into")
        if i + 1 >= len(args):
            print(
                "install-bundle: --into requires a project path",
                file=sys.stderr,
            )
            sys.exit(2)
        into = args[i + 1]
    if shutil.which("uv") is None:
        print(
            "[install-bundle] uv not on PATH; install uv first",
            file=sys.stderr,
        )
        sys.exit(2)

    if into:
        proj = Path(into).expanduser().resolve()
        venv = proj / ".venv"
        py = venv / "bin" / "python"
        if not py.exists():
            print(
                f"[install-bundle] no .venv at {venv}\n"
                f"  -> run: fantastic install {proj}     (creates .venv)\n"
                f"     then retry this command.",
                file=sys.stderr,
            )
            sys.exit(2)
        target = str(py)
        target_label = f"{proj}/.venv"
    else:
        target = sys.executable
        target_label = "kernel venv (sys.executable)"

    cmd = ["uv", "pip", "install", "--python", target, spec]
    print(f"[install-bundle] target = {target_label}", file=sys.stderr)
    print(f"[install-bundle] {' '.join(cmd)}", file=sys.stderr)
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        print(r.stderr.strip(), file=sys.stderr)
        sys.exit(r.returncode)
    if r.stdout.strip():
        print(r.stdout.strip(), file=sys.stderr)
    print(
        "[install-bundle] done. Restart any running daemon "
        "so the new entry point is discovered.",
        file=sys.stderr,
    )


# ─── default long-running mode ───────────────────────────────────


async def _default(kernel) -> None:
    """Compose & run the long-running default. Acquires the lock for
    the lifetime of the process — refuses cleanly if another fantastic
    owns this project dir.

    Composition:
      - web on disk → uvicorn binds + blocks via _block_forever
      - tty stdin → REPL stdin loop
      - neither → exit silently (no lock acquired)
    """
    web_mod = _find_bundle_module("web", ctx=kernel)
    web_agents = (
        [a for a in kernel.agents.values() if a.handler_module == web_mod]
        if web_mod
        else []
    )
    has_repl = sys.stdin.isatty()

    if not web_agents and not has_repl:
        return  # nothing to do — exit silently, no lock

    # Lock-first — before booting (so port binds don't happen on conflict).
    with FantasticLock():
        await _boot_all_agents(kernel)
        print("[kernel] up", flush=True)

        tasks: list[Any] = []
        if web_agents:
            tasks.append(_block_forever())
        if has_repl:
            tasks.append(_repl_loop(kernel))

        try:
            await asyncio.gather(*tasks)
        except (KeyboardInterrupt, asyncio.CancelledError):
            pass


async def _block_forever() -> None:
    try:
        while True:
            await asyncio.sleep(3600)
    except (KeyboardInterrupt, asyncio.CancelledError):
        pass


async def _repl_loop(kernel) -> None:
    """Interactive stdin loop. Exits on EOF / `exit` / `quit`."""
    while True:
        try:
            line = await _read_line("fantastic> ")
        except (EOFError, KeyboardInterrupt):
            print()
            break
        line = line.strip()
        if not line:
            continue
        if line in ("exit", "quit"):
            break
        if line == "list":
            for a in kernel.list():
                tag = " (singleton)" if a.get("singleton") else ""
                print(f"  {a['id']}{tag}  →  {a.get('handler_module', '<root>')}")
            continue
        if line.startswith("add "):
            parts = shlex.split(line[4:])
            if not parts:
                print("  usage: add <bundle> [k=v ...]")
                continue
            bundle = parts[0]
            handler_module = _find_bundle_module(bundle, ctx=kernel)
            if handler_module is None:
                print(f"  unknown bundle {bundle!r}")
                continue
            meta: dict[str, Any] = {}
            for p in parts[1:]:
                if "=" in p:
                    k2, v = p.split("=", 1)
                    meta[k2] = _coerce(v)
            r = kernel.create(handler_module, **meta)
            if isinstance(r, dict) and "id" in r:
                await kernel.send(r["id"], {"type": "boot"})
            await _print_result(r)
            continue
        if line.startswith("delete "):
            r = await kernel.delete(line[7:].strip())
            await _print_result(r)
            continue
        if line.startswith("@"):
            target, body = _parse_at(line)
            if not target:
                print("  usage: @<id> <text> | @<id> <verb> k=v ...")
                continue
            r = await kernel.send(target, body)
            await _print_result(r)
            continue
        print(
            f"  unknown command: {line!r}  "
            "(try: list, add <bundle>, delete <id>, @<id> ...)"
        )


# ─── helpers ─────────────────────────────────────────────────────


async def _boot_all_agents(kernel) -> None:
    """Send `{type:"boot"}` to every agent in the tree so bundles
    hydrate process-memory state (PTYs, uvicorn, HTTP clients).
    Order: registration order (Python dict order, root first)."""
    for a in list(kernel.agents.values()):
        try:
            await kernel.send(a.id, {"type": "boot"})
        except Exception as e:
            print(f"  [kernel] boot {a.id!r} raised: {e}", file=sys.stderr)


def _coerce(v: str) -> Any:
    if v.lower() in ("true", "false"):
        return v.lower() == "true"
    try:
        return int(v)
    except ValueError:
        pass
    try:
        return float(v)
    except ValueError:
        pass
    if (v.startswith("{") and v.endswith("}")) or (
        v.startswith("[") and v.endswith("]")
    ):
        try:
            return json.loads(v)
        except json.JSONDecodeError:
            pass
    return v


def _parse_kv(args: list[str]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for a in args:
        if "=" not in a:
            continue
        k, v = a.split("=", 1)
        out[k] = _coerce(v)
    return out


def _parse_at(line: str) -> tuple[str, dict]:
    """Parse `@<id> <text>` or `@<id> <verb> [k=v ...]` → (id, payload)."""
    rest = line[1:].strip()
    if not rest:
        return "", {}
    parts = shlex.split(rest)
    target = parts[0]
    args = parts[1:]
    if not args:
        return target, {"type": "send", "text": ""}
    if len(args) == 1 and "=" not in args[0]:
        return target, {"type": args[0]}
    if any("=" in a for a in args):
        verb = args[0] if "=" not in args[0] else "send"
        kv_args = args[1:] if "=" not in args[0] else args
        kv = {k: _coerce(v) for k, v in (a.split("=", 1) for a in kv_args if "=" in a)}
        return target, {"type": verb, **kv}
    text = rest[len(target) :].strip()
    return target, {"type": "send", "text": text}


async def _print_result(result: Any) -> None:
    if result is None:
        return
    if isinstance(result, dict):
        if "error" in result:
            print(f"  error: {result['error']}")
            return
        if "id" in result and "handler_module" in result:
            print(f"  created {result['id']}")
            return
    try:
        print(f"  {json.dumps(result, indent=2, default=str)}")
    except (TypeError, ValueError):
        print(f"  {result}")


async def _read_line(prompt: str) -> str:
    loop = asyncio.get_running_loop()
    return await loop.run_in_executor(None, input, prompt)


def _print_help() -> None:
    """Print the CLI cheatsheet — `cli/help.md`. It lives in the `cli`
    bundle because the CLI is what renders it: file-backed, editable
    markdown, not a hardcoded string. Points at `fantastic reflect
    return_readme=true` for the live-system bootstrap."""
    import importlib.resources

    try:
        src = importlib.resources.files("cli") / "help.md"
        print(src.read_text(encoding="utf-8").rstrip())
    except (ModuleNotFoundError, FileNotFoundError, OSError, TypeError):
        print("fantastic — run `fantastic reflect return_readme=true` to bootstrap.")
