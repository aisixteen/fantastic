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
import sys

from core import Core

from kernel import Kernel, _load_dotenv


def main_dispatch() -> None:
    n = _load_dotenv()
    if n:
        print(f"[kernel] loaded {n} var(s) from .env", file=sys.stderr)
    kernel = Kernel()
    core = Core(kernel, argv=sys.argv[1:])
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
