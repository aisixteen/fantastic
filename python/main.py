"""Fantastic kernel — CLI entry. Composes the substrate + the root loader.

    Kernel()                              the shared context + tree mgmt
      └─ fs_loader  (id="fs_loader")           the persistence/hydration ROOT
            └─ Cli(kernel, parent=root)   stdout renderer (only if tty)

The ROOT agent IS an `fs_loader` (`id="fs_loader"`, `handler_module=
"fs_loader.tools"`): it owns the `.fantastic/` medium. Bootstrap reads
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

from fs_loader.tools import read_tree, write_record

from kernel import Kernel, _load_dotenv, dispatch_argv


def _bootstrap(kernel: Kernel) -> None:
    """Build the live tree from `.fantastic` (or seed a fresh root) and
    compose per-process ephemerals. No agent boots here — `dispatch_argv`
    runs the boot pass (which is when the root loader subscribes)."""
    root_dir = Path(".fantastic")
    records = read_tree(root_dir)
    seeded = not records
    if seeded:
        # Fresh project: the root IS the loader (id="fs_loader"). Seed its
        # agent.json + readme now so the next boot reads it (read-only
        # `reflect` paths take no lock and write nothing).
        records = [{"id": "fs_loader", "handler_module": "fs_loader.tools"}]
    kernel.load(records, root_path=root_dir)
    if seeded:
        write_record(kernel.root._root_path, kernel.root.record)
    if sys.stdin.isatty():
        from cli import Cli

        Cli(kernel, parent=kernel.root)  # ephemeral stdout renderer


def main_dispatch() -> None:
    n = _load_dotenv()
    if n:
        print(f"[kernel] loaded {n} var(s) from .env", file=sys.stderr)
    kernel = Kernel()
    _bootstrap(kernel)

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

    try:
        asyncio.run(dispatch_argv(kernel, sys.argv[1:]))
    except KeyboardInterrupt:
        pass
    except RuntimeError as e:
        # Lock conflict raises here — print + exit 1.
        print(f"[kernel] {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main_dispatch()
