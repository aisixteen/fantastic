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

import yaml

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
#: EMBEDDED META BUNDLES: a product's own `bundle.yaml` text compiled into the host
#: (`register_meta_yaml`) — the same rows as a folder's meta bundle, read from no
#: folder. A folder of the same name overrides it.
_EMBEDDED: dict[str, dict] = {}


#: Bundles that load ONLY when the host turns them on: handler module -> the
#: environment variable that does. Off, the module is not in the catalog (a create
#: of it is an unknown handler, as on a rust host that did not register it).
_GATED: dict[str, str] = {}


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
    if name in _FOLDER or _folder_meta(name) is not None:
        return
    _FOLDER_ROWS[name] = row


def folder_rows() -> list[dict]:
    """Every bundle this kernel read from a folder or has embedded — loaded and
    refused — by name, each with its `source`. A folder's refusal over an embedded
    bundle of the same name rides on the embedded row as `folder_error` (the embedded
    one stays live). A meta bundle's row is listed without its `record` (the fields, a
    directive among them, are the instances' business, not the catalog's)."""
    out: dict[str, dict] = {n: {**r, "source": "embedded"} for n, r in _EMBEDDED.items()}
    for n, row in _FOLDER_ROWS.items():
        if "error" in row and n in out:
            out[n]["folder_error"] = row["error"]
        else:
            out[n] = {**row, "source": "folder"}
    return [{k: v for k, v in out[n].items() if k != "record"} for n in sorted(out)]


def register_meta(name: str, row: dict) -> None:
    """Load a META BUNDLE: a `bundle.yaml` with a `base` (a registered
    `handler_module`, or another meta bundle) and no program. Its row, with the
    `base` and the `record` its instances inherit, is all there is of it: nothing is
    built or registered, and `import_handler` resolves `name` to the base."""
    _FOLDER.pop(name, None)
    _FOLDER_ROWS[name] = row


def folder_bases() -> list[str]:
    """The installed bundled agents a folder can extend (they build `folder_bundle`)."""
    out = []
    for ep in entry_points(group=BUNDLE_ENTRY_GROUP):
        try:
            mod = import_handler(ep.value)
        except Exception:  # noqa: BLE001 — a bundle that cannot import is no base
            continue
        if hasattr(mod, "folder_bundle"):
            out.append(ep.name)
    return sorted(out)


def _folder_meta(name: str) -> dict | None:
    """The loaded FOLDER meta bundle `name`'s row, if it is one."""
    row = _FOLDER_ROWS.get(name)
    if row is None or "error" in row or "record" not in row:
        return None
    return row


def _meta_row_of(name: str) -> dict | None:
    """The row of the meta bundle `name` — a folder's over an embedded one's."""
    if name in _FOLDER:
        return None
    return _folder_meta(name) or _EMBEDDED.get(name)


def _meta_base(name: str) -> str | None:
    """The `base` of the loaded meta bundle `name`, if it is one."""
    row = _meta_row_of(name)
    base = row.get("base") if row else None
    return base if isinstance(base, str) else None


def meta_row(name: str, base: str, sentence: str, spec: dict, row: dict) -> dict:
    """The one shape check of a meta bundle, whichever way it arrives (a folder's
    `bundle.yaml`, an embedded text): `record` is a mapping, `base` resolves to a
    registered bundle and leads back to nothing of this name. → its row; raises
    ValueError saying why not."""
    record = spec.get("record")
    if record is None:
        record = {}
    if not isinstance(record, dict):
        raise ValueError("record: must be a mapping of the fields every instance inherits")
    handler = handler_of(base)
    if handler is None:
        raise ValueError(
            f"base: {base!r} is no bundle of this kernel — a folder extends a registered "
            "handler_module or another folder bundle (folder bases with a program — "
            f"known: {', '.join(folder_bases()) or 'none'})"
        )
    # no cycle: following the bases from `base` never comes back to this bundle
    at = handler
    for _ in range(17):
        if at == name:
            raise ValueError(
                f"base: {base!r} leads back to {name!r} — a meta bundle cannot extend itself"
            )
        nxt = _meta_base(at)
        if nxt is None:
            break
        at = nxt
    return {**row, "base": handler, "sentence": sentence, "record": record}


def register_meta_yaml(name: str, text: str) -> None:
    """Embed a META BUNDLE from its `bundle.yaml` text: a product's own bundle,
    compiled in, read from no folder. A folder bundle of the same name overrides it.
    Raises ValueError saying why it is refused."""
    try:
        spec = yaml.safe_load(text)
    except yaml.YAMLError as e:
        raise ValueError(f"{name}: {e}") from e
    base = spec.get("base") if isinstance(spec, dict) else None
    if not isinstance(base, str) or not base:
        raise ValueError(f"{name}: base: required — the bundle it extends")
    sentence = spec.get("sentence")
    if not isinstance(sentence, str) or not sentence.strip():
        raise ValueError(f"{name}: sentence: required — one line saying what it is")
    try:
        row = meta_row(
            name, base, sentence.strip(), spec, {"name": name, "handler_module": name}
        )
    except ValueError as e:
        raise ValueError(f"{name}: {e}") from e
    _EMBEDDED[name] = row


def meta_chain(handler_module: str) -> list[str]:
    """The meta bundles `handler_module` goes through on its way to the compiled
    base, itself first. Empty for anything that is not a meta bundle."""
    chain: list[str] = []
    at = handler_module
    while (base := _meta_base(at)) is not None:
        if at in chain or len(chain) > 16:
            break
        chain.append(at)
        at = base
    return chain


def defaults(handler_module: str) -> dict:
    """The fields an instance of `handler_module` inherits, whatever its source: a
    compiled bundle gives none; a folder bundle its `record`; a meta bundle its own
    and each bundle's on the way to the compiled base — the nearest winning."""
    chain = meta_chain(handler_module)
    last = _meta_base(chain[-1]) if chain else handler_module
    if last in _FOLDER:
        chain.append(last)
    out: dict = {}
    for name in reversed(chain):
        row = _meta_row_of(name) or _FOLDER_ROWS.get(name) or {}
        out.update(row.get("record") or {})
    return out


def base_of(name: str) -> str:
    """The compiled (or program folder) bundle a meta bundle runs on, at the end of its
    chain; `name` itself when it is no meta bundle."""
    chain = meta_chain(name)
    return (_meta_base(chain[-1]) or name) if chain else name


def handler_of(base: str) -> str | None:
    """The handler module a `base:` names: a registered `handler_module`, a loaded
    meta bundle, or an installed bundle's name (`file_bridge` → `file_bridge.tools`)."""
    if base in _UNLISTED or base in _FOLDER or _meta_base(base) is not None or _in_catalog(base):
        return base
    for ep in entry_points(group=BUNDLE_ENTRY_GROUP):
        if ep.name == base and bundle_enabled(ep.value):
            return ep.value
    return None


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
    # a META BUNDLE resolves to its base: an instance of it IS an instance of the
    # base, with the meta bundle's fields under its own (`defaults`)
    base = _meta_base(handler_module)
    if base is not None:
        return import_handler(base)
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
    base = _meta_base(handler_module)
    if base is not None:
        return bundle_package(base)  # a meta bundle reads its base's resources
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
