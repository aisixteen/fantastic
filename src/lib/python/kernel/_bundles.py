"""Bundle discovery via entry points — and THE ONE WAY a handler module is reached.

`fantastic.bundles` — every installed bundle (name → handler module).
Used by the REPL `add <name>` flow to resolve a human-typed bundle name
to its dotted module path, and by `import_handler`, which loads a record's
`handler_module` ONLY when the catalog (or `register_unlisted`) names it.

A bundle is anything with a `handler(id, payload, agent)` callable in
its declared module. Construction is explicit (e.g. `Cli(kernel,
parent=kernel_state)`); the substrate never instantiates a bundle by name.
"""

from __future__ import annotations

import importlib
import os
import sys
from importlib.metadata import entry_points
from types import ModuleType

from kernel._kernel import BUNDLE_ENTRY_GROUP, Kernel

#: Test/probe bundles that DISPATCH but are not installed (`register_unlisted`).
_UNLISTED: dict[str, ModuleType] = {}
#: Handler modules the entry-point catalog has named, and names it did not. A name
#: never seen re-scans once (a bundle installed while the process runs resolves on
#: its first lookup); a known miss does not — a record of another runtime (`*.ts`)
#: is looked up on every call and every reflect node, and a scan costs ~1.4 ms.
_CATALOG: set[str] = set()
_MISSING: set[str] = set()
#: FOLDER BUNDLES: `<root>/bundles/<name>/` — code that EXTENDS
#: a bundled agent (`base`), loaded by this kernel's loader from its own root. The
#: handler_module of an instance is the folder name. One kernel per process, so
#: process-wide like the catalog. `_FOLDER_ROWS` is what `reflect bundles=all`
#: lists — a refused folder too, with its `error`, never loaded.
_FOLDER: dict[str, ModuleType] = {}
_FOLDER_ROWS: dict[str, dict] = {}


#: Bundles that load ONLY when the host turns them on: handler module -> the
#: environment variable that does. Off, the module is not in the catalog (a create
#: of it is an unknown handler, as on a rust host that did not register it).
_GATED: dict[str, str] = {
    "agentic_resources.tools": "FANTASTIC_AGENTIC_RESOURCES",
    "task_registry.tools": "FANTASTIC_TASK_REGISTRY",
}


def bundle_enabled(handler_module: str) -> bool:
    """Is this bundle on? Every bundle is, except a gated one whose environment
    variable is not truthy (`1`, `true`, `yes`, `on`)."""
    var = _GATED.get(handler_module)
    if var is None:
        return True
    return os.environ.get(var, "").strip().lower() in ("1", "true", "yes", "on")


def register_unlisted(handler_module: str, module: ModuleType) -> None:
    """Register a bundle that DISPATCHES but is not installable — a test or probe
    module with no `fantastic.bundles` entry point (the pytest fixtures, the e2e
    scenario probes). It never appears in the `bundles` catalog. Mirror of rust's
    `BundleRegistry::register_unlisted`: the same
    name, the same meaning, process-wide as python's catalog is."""
    _UNLISTED[handler_module] = module


def register_folder(name: str, module: ModuleType, row: dict) -> None:
    """Load a folder bundle: `name` (its folder) resolves to `module`, the module
    its BASE built from the folder (`folder_bundle`). Replaces a previous load —
    a reload is a re-register."""
    _FOLDER[name] = module
    _FOLDER_ROWS[name] = row


def refuse_folder(name: str, row: dict) -> None:
    """A folder that could not load: listed (with its `error`), never resolvable —
    unless an earlier load stands: a failed RELOAD keeps it (module and row), so its
    live instances keep running it; the error goes to the reload's caller."""
    if name in _FOLDER:
        return
    _FOLDER_ROWS[name] = row


def folder_rows() -> list[dict]:
    """Every folder bundle this kernel read — loaded and refused — by name."""
    return [dict(_FOLDER_ROWS[n]) for n in sorted(_FOLDER_ROWS)]


def is_folder(handler_module: str) -> bool:
    return handler_module in _FOLDER


def clashes(name: str) -> bool:
    """Is `name` a built-in bundle's name, handler module or package? A folder may
    not shadow one."""
    if name in _UNLISTED:
        return True
    for ep in entry_points(group=BUNDLE_ENTRY_GROUP):
        if name in (ep.name, ep.value, ep.value.split(".")[0]):
            return True
    return False


def import_handler(handler_module: str) -> ModuleType:
    """The module a record's `handler_module` names — resolved ONLY from the
    `fantastic.bundles` catalog plus `register_unlisted`, never from all of
    `sys.path`.

    Importing any name on `sys.path` would run that module's top-level code on a
    grant-less `create_agent`: an agent could name `unittest.__main__` and exit
    the process, or a module it had written itself.
    A name outside the catalog is, to this kernel, a module that does not exist:
    `ModuleNotFoundError: No module named '<top-level package>'` — rust's
    text for an unregistered name, so every runtime says one thing."""
    mod = _UNLISTED.get(handler_module) or _FOLDER.get(handler_module)
    if mod is not None:
        return mod
    if not _in_catalog(handler_module):
        top = handler_module.split(".")[0]
        raise ModuleNotFoundError(f"No module named {top!r}", name=top)
    return importlib.import_module(handler_module)


def _in_catalog(handler_module: str) -> bool:
    """Does the `fantastic.bundles` catalog name `handler_module`?"""
    if not bundle_enabled(handler_module):
        return False
    if handler_module not in _CATALOG:
        if handler_module not in _MISSING:
            _CATALOG.update(ep.value for ep in entry_points(group=BUNDLE_ENTRY_GROUP))
        if handler_module not in _CATALOG:
            _MISSING.add(handler_module)
            return False
    return True


def bundle_package(handler_module: str) -> str | None:
    """The top-level package a bundle's shipped files (`readme.md`, `docs/`) are
    read from — for a CATALOG bundle, or a `register_unlisted` one whose package
    is already imported — else None: reading a package's resources imports its
    `__init__`, so a name outside the catalog is never imported to read nothing."""
    top = handler_module.split(".")[0]
    if handler_module in _FOLDER:
        return None  # a folder bundle ships no package resources
    if handler_module in _UNLISTED:
        return top if top in sys.modules else None
    return top if _in_catalog(handler_module) else None


def _find_bundle_module(name: str, ctx: Kernel | None = None) -> str | None:
    """Resolve a bundle name to its handler_module via entry points.

    Bundles publish themselves at install time via:
        [project.entry-points."fantastic.bundles"]
        <name> = "<package>.tools"

    When `ctx` is provided, the lookup goes through and populates
    `ctx.bundle_resolver`. Without ctx, it always re-walks
    `entry_points()` — fine for one-shot CLI uses; the cache matters
    only for hot-path repeated lookups inside a live serve.
    """
    if ctx is not None and name in ctx.bundle_resolver:
        return ctx.bundle_resolver[name]
    found: str | None = None
    for ep in entry_points(group=BUNDLE_ENTRY_GROUP):
        if ep.name == name:
            found = ep.value
            break
    if ctx is not None and found is not None:
        ctx.bundle_resolver[name] = found
    return found
