"""Fantastic kernel — CLI entry + match/case router.

The Kernel class and every `cmd_*` handler live in the `kernel/`
package. This file is just the visible router: parse argv, dispatch
to a subcommand, exit. Run: `python kernel.py` from this directory.

(Python prefers the `kernel/` package over this script for `import
kernel`, so external code imports work as before. Direct invocation
— `python kernel.py serve` — still routes through this file.)
"""

from __future__ import annotations

import asyncio
import sys
from typing import Any

from kernel import (
    _coerce,
    _load_dotenv,
    cmd_call,
    cmd_install,
    cmd_install_bundle,
    cmd_reflect,
    cmd_repl,
    cmd_serve,
)


def _parse_kv(args: list[str]) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for a in args:
        if "=" not in a:
            continue
        k, v = a.split("=", 1)
        out[k] = _coerce(v)
    return out


def main_dispatch() -> None:
    # Autoload .env from cwd into os.environ before anything that might
    # need it (provider keys, model overrides). Shell vars win.
    n_env = _load_dotenv()
    if n_env:
        print(f"[kernel] loaded {n_env} var(s) from .env", file=sys.stderr)
    argv = sys.argv[1:]
    if not argv:
        asyncio.run(cmd_repl())
        return
    sub, *rest = argv
    try:
        match sub:
            case "serve":
                port: int | None = None
                for i, a in enumerate(rest):
                    if a.startswith("--port="):
                        port = int(a.split("=", 1)[1])
                    elif a == "--port" and i + 1 < len(rest):
                        port = int(rest[i + 1])
                asyncio.run(cmd_serve(port))

            case "call":
                if len(rest) < 2:
                    print(
                        "usage: kernel.py call <target_id> <verb> [k=v ...]",
                        file=sys.stderr,
                    )
                    sys.exit(2)
                target, verb, *kv = rest
                asyncio.run(cmd_call(target, verb, _parse_kv(kv)))

            case "reflect":
                asyncio.run(cmd_reflect(rest[0] if rest else "kernel"))

            case "install":
                # Per-project detached venv via uv. Creates
                # <project>/.venv, installs requested packages, and
                # points python_runtime records in
                # <project>/.fantastic/agents/ at the new venv.
                # Projects without `.venv` keep using sys.executable
                # (the kernel's installed runtime) — fall-through is
                # automatic in _resolve_python.
                if not rest:
                    print(
                        "usage: kernel.py install <project_dir> [pkg ...]",
                        file=sys.stderr,
                    )
                    sys.exit(2)
                cmd_install(rest[0], list(rest[1:]))

            case "install-bundle":
                # Pip-install a third-party fantastic bundle (or any
                # package that declares `[project.entry-points."fantastic.bundles"]`)
                # into either the kernel's venv (default) or a
                # specific project's `.venv` (`--into <project>`).
                # uv pip install handles git URLs natively:
                #   git+https://github.com/u/r        — main / default branch
                #   git+https://github.com/u/r@v0.1   — tag
                #   git+https://github.com/u/r@feat   — branch
                #   git+https://github.com/u/r@a3f2b1 — commit
                if not rest:
                    print(
                        "usage: kernel.py install-bundle <spec> [--into <project>]\n"
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
                cmd_install_bundle(spec, into)

            case "repl" | "shell":
                asyncio.run(cmd_repl())

            case "-h" | "--help" | "help":
                print(
                    "fantastic kernel\n"
                    "  python kernel.py                       # interactive REPL (default)\n"
                    "  python kernel.py serve [--port N]     # headless: web agent on port; --port omitted → ephemeral free port\n"
                    "  python kernel.py call <id> <verb> [k=v ...]   # one-shot RPC, print JSON, exit\n"
                    "  python kernel.py reflect [<id>]        # shorthand: call <id> reflect (default kernel)\n"
                    "  python kernel.py install <project_dir> [pkg ...]   # uv venv <dir>/.venv + install pkgs + point python_runtime records at it\n"
                    "  python kernel.py install-bundle <spec> [--into <project>]   # uv pip install a fantastic bundle (git URL / pypi / path) into kernel venv or project's .venv"
                )

            case _:
                print(
                    f"unknown subcommand {sub!r} "
                    "(try: serve, call, reflect, repl, install, install-bundle)",
                    file=sys.stderr,
                )
                sys.exit(2)
    except KeyboardInterrupt:
        pass
    except RuntimeError as e:
        # Lock conflict on `serve` raises here — print and exit 1 cleanly.
        print(f"[serve] {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main_dispatch()
