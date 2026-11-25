"""harness/helper_imports — the suite's own imports, with no model and no kernel.

A name listed in `helpers.__all__` / `helpers._LAZY` that its module does not
define fails only when a probe imports it, at the start of a model run, as a
`no verdict` with an ImportError. This checks it for free:
  a) every name `helpers` exports (`__all__` and the lazy map) resolves;
  b) every `from helpers import …` / `from helpers.<mod> import …` in every
     `probe.py` under `tests/` and every module under `helpers/` names something
     that module defines. The probe files are PARSED, not run: a probe resolves its
     model plug at import time, and this test runs none."""

from __future__ import annotations

import ast
import importlib
import sys
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

import helpers  # noqa: E402  (its import puts the kernel tree on sys.path)
from helpers.verdict import verdict  # noqa: E402

bad: list[str] = []


def need(cond: bool, what: str) -> None:
    print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
    if not cond:
        bad.append(what)


def resolves(module: str, name: str) -> str | None:
    """None when `from <module> import <name>` works, else the error. As Python
    does it: an attribute of the module, else its submodule `<module>.<name>`."""
    try:
        mod = importlib.import_module(module)
        if hasattr(mod, name):
            return None
        importlib.import_module(f"{module}.{name}")
    except Exception as e:  # noqa: BLE001  (any failure is the finding)
        return f"{type(e).__name__}: {e}"
    return None


def helper_imports(path: Path) -> list[tuple[int, str, str]]:
    """(line, module, name) of every `from helpers[.x] import name` in `path`."""
    out = []
    for node in ast.walk(ast.parse(path.read_text(), str(path))):
        if isinstance(node, ast.ImportFrom) and node.level == 0 and node.module and (
            node.module == "helpers" or node.module.startswith("helpers.")
        ):
            out.extend((node.lineno, node.module, a.name) for a in node.names if a.name != "*")
    return out


def main() -> int:
    # a) the package's own exports
    names = sorted(set(helpers.__all__) | set(helpers._LAZY))
    for name in names:
        err = resolves("helpers", name)
        need(err is None, f"helpers.{name}" + (f" — {err}" if err else ""))

    # b) every probe and every helper module, by what it imports
    files = sorted(E2E.glob("tests/**/probe.py")) + sorted(E2E.glob("helpers/**/*.py"))
    checked = 0
    for f in files:
        for line, module, name in helper_imports(f):
            checked += 1
            err = resolves(module, name)
            if err is not None:
                need(False, f"{f.relative_to(E2E)}:{line}: from {module} import {name} — {err}")
    need(checked > 0, f"{checked} helper imports in {len(files)} files resolve")

    verdict(not bad, f"{len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
