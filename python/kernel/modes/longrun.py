"""Long-running default mode: boot agents, REPL loop, signal-driven shutdown."""

from __future__ import annotations

import asyncio
import shlex
import signal
import sys
from typing import Any

from kernel._bundles import _find_bundle_module
from kernel._lock import FantasticLock

from kernel.modes.parse import _coerce, _parse_at
from kernel.modes.print import _print_result, _read_line


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

        # Graceful-shutdown plumbing: SIGTERM / SIGINT / SIGHUP set
        # the stop event, the `wait(FIRST_COMPLETED)` returns, and the
        # `finally` block walks the tree calling each bundle's
        # `on_shutdown` / `on_delete` hook so PTYs, `code serve-web`,
        # uvicorn etc. die before the kernel exits — instead of
        # being left as orphans for the user to clean up by hand.
        stop = asyncio.Event()
        # Publish the SAME event on the kernel ctx so the root-only
        # `shutdown_kernel` verb can trigger this exact graceful path
        # remotely (over web_rest / web_ws), not just via an OS signal.
        kernel.shutdown_event = stop
        loop = asyncio.get_running_loop()

        def _request_stop(sig_name: str) -> None:
            if not stop.is_set():
                print(
                    f"\n[kernel] {sig_name} — shutting down...",
                    file=sys.stderr,
                    flush=True,
                )
                stop.set()

        for sig_name in ("SIGINT", "SIGTERM", "SIGHUP"):
            sig = getattr(signal, sig_name, None)
            if sig is None:
                continue
            try:
                loop.add_signal_handler(sig, _request_stop, sig_name)
            except NotImplementedError:
                # Windows / non-Unix loops can't install signal
                # handlers — KeyboardInterrupt still propagates.
                pass

        tasks: list[asyncio.Task] = []
        if web_agents:
            tasks.append(asyncio.create_task(_block_forever()))
        if has_repl:
            tasks.append(asyncio.create_task(_repl_loop(kernel)))
        stop_task = asyncio.create_task(stop.wait())

        try:
            try:
                await asyncio.wait(
                    [*tasks, stop_task],
                    return_when=asyncio.FIRST_COMPLETED,
                )
            except (KeyboardInterrupt, asyncio.CancelledError):
                pass
            # Whatever was still running (the other branch + stop_task
            # if a real task finished first) — cancel and drain.
            for t in [*tasks, stop_task]:
                if not t.done():
                    t.cancel()
            await asyncio.gather(*tasks, stop_task, return_exceptions=True)
        finally:
            # Always-run cleanup: depth-first call each agent's
            # `on_shutdown` / `on_delete` hook. Idempotent — `Kernel.
            # shutdown` flips `_shutdown_complete` so the atexit
            # safety net in main.py is a no-op when the daemon
            # exited cleanly through here.
            print("[kernel] tearing down agents...", file=sys.stderr, flush=True)
            try:
                await kernel.shutdown()
            except Exception as e:
                print(f"[kernel] shutdown raised: {e}", file=sys.stderr)
            print("[kernel] down", file=sys.stderr, flush=True)


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


async def _boot_all_agents(kernel) -> None:
    """Send `{type:"boot"}` to every agent in the tree so bundles
    hydrate process-memory state (PTYs, uvicorn, HTTP clients).
    Order: registration order (Python dict order, root first)."""
    for a in list(kernel.agents.values()):
        try:
            await kernel.send(a.id, {"type": "boot"})
        except Exception as e:
            print(f"  [kernel] boot {a.id!r} raised: {e}", file=sys.stderr)
