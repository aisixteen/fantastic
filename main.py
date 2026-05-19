"""Fantastic kernel — CLI entry. Composes substrate + core.

    Kernel()                        the shared context + tree mgmt
        └─ Core(kernel, argv)       userland orchestrator at id="core"
              └─ Cli(kernel, parent=core)   (only if stdin is a tty)

`Core.run()` delegates to `kernel.dispatch_argv(argv)`. The substrate
handles both one-shot subcommands (`<id> <verb>` / `reflect` /
`install` / `install-bundle`) and the default long-running mode
(composes web@port if `--port N` is passed; runs REPL if stdin is a
tty; blocks while either runs). main.py is the composition root; the
substrate does the work.
"""

from __future__ import annotations

import asyncio
import atexit
import sys

from core import Core

from kernel import Kernel, _load_dotenv


def main_dispatch() -> None:
    n = _load_dotenv()
    if n:
        print(f"[kernel] loaded {n} var(s) from .env", file=sys.stderr)
    kernel = Kernel()
    core = Core(kernel, argv=sys.argv[1:])

    # atexit safety net for graceful shutdown. The primary path is
    # signal handlers + the `finally` block inside `_default` — those
    # walk the tree depth-first and call each bundle's `on_shutdown`/
    # `on_delete` hook (kills `code serve-web`, PTYs, etc.) before the
    # daemon exits. This atexit covers the OTHER ways Python can leave
    # the long-running loop: an uncaught exception that escapes
    # asyncio.run, a `sys.exit` from somewhere unexpected, a one-shot
    # subcommand that spawned background tasks. `Kernel.shutdown` is
    # guarded by `_shutdown_complete`, so when the in-loop finally
    # already ran this is a no-op.
    def _atexit_shutdown() -> None:
        if kernel._shutdown_complete or kernel.root is None:
            return
        try:
            asyncio.run(kernel.shutdown())
        except RuntimeError:
            # A loop is still running, or one was already closed and
            # asyncio.run can't start a fresh one — bail. The
            # in-loop finally is the real cleanup path; this is
            # best-effort.
            pass
        except Exception as e:
            print(f"[atexit] shutdown raised: {e}", file=sys.stderr)

    atexit.register(_atexit_shutdown)

    try:
        asyncio.run(core.run())
    except KeyboardInterrupt:
        pass
    except RuntimeError as e:
        # Lock conflict from `--port` raises here — print + exit 1.
        print(f"[kernel] {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main_dispatch()
