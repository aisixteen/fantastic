"""Shared helpers + the ONE path bootstrap every runner starts with.

Import this first, from any depth:

    from helpers import bootstrap; bootstrap()

Why a marker file instead of `parents[N]`: a probe that hard-codes its own
depth (`parents[1] / "src" / "lib" / "python"`) silently breaks when it moves
into another folder.
"""

from __future__ import annotations

import sys
from pathlib import Path

__all__ = [
    "BASELINE",
    "HUMAN_ANSWER",
    "Connected",
    "Limits",
    "ProxyReader",
    "Watch",
    "bootstrap",
    "box",
    "connect",
    "distinct_calls",
    "drive",
    "e2e_root",
    "human",
    "repo_root",
    "verdict",
]


def e2e_root(start: Path | None = None) -> Path:
    """The `e2e/` dir, found by its marker rather than by counting parents."""
    p = (start or Path(__file__)).resolve()
    for cand in (p, *p.parents):
        if (cand / ".e2e-root").exists():
            return cand
    raise RuntimeError("no .e2e-root marker above " + str(p))


def repo_root() -> Path:
    return e2e_root().parent


def bootstrap() -> None:
    """Put the kernel and this suite on `sys.path`. Idempotent."""
    e2e = e2e_root()
    for p in (str(e2e), str(repo_root() / "src" / "lib" / "python")):
        if p not in sys.path:
            sys.path.insert(0, p)


bootstrap()

# LAZY re-exports. The names below reach into the AI machinery (`ai_core`, the
# backends) and the contract-corpus runner needs NONE of it — that half of this
# package is free and non-LLM. Importing them eagerly made `helpers` unusable
# without the model stack installed, which is the wrong dependency to impose on
# a suite that never calls a model.
_LAZY = {
    "Watch": "helpers.watch",
    "Limits": "helpers.proxy",
    "ProxyReader": "helpers.proxy",
    "box": "helpers.proxy",
    "distinct_calls": "helpers.proxy",
    "BASELINE": "helpers.ai",
    "Connected": "helpers.ai",
    "connect": "helpers.ai",
    "drive": "helpers.ai",
    "human": "helpers.ai",
    "HUMAN_ANSWER": "helpers.ai",
}


# EAGER, deliberately: `helpers.verdict` is also a MODULE name, so a lazy entry
# resolves to the module once anything has imported it and to the function
# otherwise — `from helpers import verdict` would mean two different things
# depending on import order. It has no heavy dependencies, so it is just bound.
from helpers.verdict import verdict


def __getattr__(name: str):
    if name in _LAZY:
        import importlib

        return getattr(importlib.import_module(_LAZY[name]), name)
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


def __dir__():
    return sorted(set(globals()) | set(_LAZY))
