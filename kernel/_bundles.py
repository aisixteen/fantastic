"""Bundle discovery via entry points.

`fantastic.bundles` — every installed bundle (name → handler module).
Used by REPL `add <name>` and HTTP `install-bundle` flows to resolve a
human-typed bundle name to its dotted module path. Substrate doesn't
care which bundles exist; this is purely for the lookup convenience.

A bundle is anything with a `handler(id, payload, agent)` callable in
its declared module. Construction is explicit (e.g. `Cli(kernel,
parent=core)`); the substrate never instantiates a bundle by name.
"""

from __future__ import annotations

from importlib.metadata import entry_points

from kernel._kernel import BUNDLE_ENTRY_GROUP, Kernel


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
