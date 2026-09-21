"""Fantastic kernel — CLI entry. Composes the substrate + the root loader.

    Kernel()                              the shared context + tree mgmt
      └─ kernel_state  (id="kernel_state")           the persistence/hydration ROOT
            └─ Cli(kernel, parent=root)   stdout renderer (only if tty)

The ROOT agent IS an `kernel_state` (`id="kernel_state"`, `handler_module=
"kernel_state.tools"`): it owns the `.fantastic/` medium. Bootstrap reads
the on-disk tree (`read_tree`), rebuilds it in memory (`kernel.load`),
then hands argv to `dispatch_argv`. On the long-running / one-shot-call
paths the root loader boots — subscribing to the state stream and
auto-persisting every mutation. Persistence is decoupled from the
kernel: `Agent` never touches disk; the loader does.
"""

from __future__ import annotations

import asyncio
import atexit
import sys
from pathlib import Path

from cli import Cli
from kernel_state.tools import hydrate_messages, read_tree, write_record

from kernel import Kernel, _load_dotenv, dispatch_argv


def _bootstrap(kernel: Kernel, root_dir: Path = Path(".fantastic")) -> None:
    """Build the live tree from `root_dir` (or seed a fresh root) and
    compose per-process ephemerals. No agent boots here — `dispatch_argv`
    runs the boot pass (which is when the root loader subscribes)."""
    records = read_tree(root_dir)
    # No ROOT record on disk (fresh project, or a missing/corrupt `agent.json`
    # with children still under `agents/`): the root IS the loader
    # (id="kernel_state"). Seed its agent.json + readme now so the next boot
    # reads it (read-only `reflect` paths take no lock and write nothing).
    seeded = not any(r.get("parent_id") is None for r in records)
    if seeded:
        records = [
            {"id": "kernel_state", "handler_module": "kernel_state.tools"}
        ] + records
    kernel.load(records, root_path=root_dir)
    # HYDRATE THE MESSAGE STORE, cold, before the first send mints an id. The kernel
    # starts empty; the loader bundle's reader fills it from above and sets the counter.
    hydrate_messages(kernel, root_dir)
    # No stream provider is composed here. kernel_state's AUTO-FLUSH persists ONLY
    # through a DISCOVERED provider (a file_bridge child rooted at `.fantastic` that an
    # operator/LLM wires) — with none wired the live tree stays in RAM (no fallback).
    # The COLD primitives are direct by design: this one-time seed write below, and
    # `read_tree` at boot — the chicken-egg bring-up before any provider can exist.
    if seeded:
        write_record(kernel.root._root_path, kernel.root.record)
    if sys.stdin.isatty():
        Cli(kernel, parent=kernel.root)  # ephemeral stdout renderer


def _build_kernel(root_dir: Path = Path(".fantastic")) -> Kernel:
    """Construct + hydrate ONE kernel from a `.fantastic` root. The seam for a
    future kernel LIST (several `Kernel()` bridged IN-PROCESS via io_bridge's
    memory transport — each seats a `MemoryTransport.pair()` half on an io_bridge
    agent). Today `main` builds exactly one; a multi-kernel launcher would call
    this per root and pick a primary for `dispatch_argv`. NOT built now."""
    kernel = Kernel()
    _bootstrap(kernel, root_dir)
    return kernel


def _ledger_cap_from(argv: list[str]) -> tuple[list[str], int | None, bool]:
    """Strip `--ledger-cap N|inf` from argv; fall back to FANTASTIC_LEDGER_CAP.
    Returns (argv-without-the-flag, cap, was_set). `inf` = unbounded (None)."""
    import os

    raw, out, i = None, [], 0
    while i < len(argv):
        if argv[i] == "--ledger-cap" and i + 1 < len(argv):
            raw = argv[i + 1]
            i += 2
            continue
        out.append(argv[i])
        i += 1
    if raw is None:
        raw = os.environ.get("FANTASTIC_LEDGER_CAP")
    if raw is None:
        return out, None, False
    if raw == "inf":
        return out, None, True
    return out, int(raw), True


def main_dispatch() -> None:
    n = _load_dotenv()
    if n:
        print(f"[kernel] loaded {n} var(s) from .env", file=sys.stderr)
    argv, cap, cap_set = _ledger_cap_from(sys.argv[1:])
    sys.argv[1:] = argv
    kernel = _build_kernel()
    if cap_set:
        kernel.ledger_cap = cap

    # atexit safety net for graceful shutdown. The primary path is
    # signal handlers + the `finally` block inside `_default` — those
    # walk the tree depth-first and call each bundle's `on_shutdown`/
    # `on_delete` hook (kills `code serve-web`, PTYs, etc.) AND flush
    # the root loader before the daemon exits. This atexit covers the
    # OTHER ways Python can leave the loop: an uncaught exception that
    # escapes asyncio.run, a `sys.exit` from somewhere unexpected.
    # `Kernel.shutdown` is guarded by `_shutdown_complete`, so when the
    # in-loop finally already ran this is a no-op.
    def _atexit_shutdown() -> None:
        if kernel._shutdown_complete or kernel.root is None:
            return
        try:
            asyncio.run(kernel.shutdown())
        except RuntimeError:
            # A loop is still running, or one was already closed and
            # asyncio.run can't start a fresh one — bail. The in-loop
            # finally is the real cleanup path; this is best-effort.
            pass
        except Exception as e:
            print(f"[atexit] shutdown raised: {e}", file=sys.stderr)

    atexit.register(_atexit_shutdown)

    async def _run() -> None:
        # SHUTDOWN ON THE LOOP THAT RAN THE CALL. A one-shot subcommand used to
        # leave `asyncio.run` and let the atexit net start a FRESH loop — where
        # the pump's tail task had died with the first one, so `catch_up` could
        # never see the counter caught up and burned its full 1000 × 0.01 s
        # bound: `kernel ps` took 11.5 s (measured 2026-09-21). Shutting down
        # inside the same loop lands every row the one-shot minted and returns
        # as soon as the log is caught up; the atexit net below then no-ops on
        # `_shutdown_complete`, and the long-running mode's own `finally` does too.
        try:
            await dispatch_argv(kernel, sys.argv[1:])
        finally:
            await kernel.shutdown()

    try:
        asyncio.run(_run())
    except KeyboardInterrupt:
        pass
    except RuntimeError as e:
        # Lock conflict raises here — print + exit 1.
        print(f"[kernel] {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main_dispatch()
