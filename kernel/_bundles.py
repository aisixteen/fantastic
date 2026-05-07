"""Bundle discovery + singleton seeding."""

from __future__ import annotations

from importlib.metadata import entry_points

from kernel._kernel import BUNDLE_ENTRY_GROUP, Kernel


def _find_bundle_module(name: str) -> str | None:
    """Resolve a bundle name to its handler_module via entry points.

    Bundles publish themselves at install time via:
        [project.entry-points."fantastic.bundles"]
        <name> = "<package>.tools"

    Discovery is uniform for built-in workspace members and pip-installed
    third-party plugins — both register through the same entry-point group.
    """
    for ep in entry_points(group=BUNDLE_ENTRY_GROUP):
        if ep.name == name:
            return ep.value
    return None


async def _seed_singletons(k: Kernel, boot_all: bool = True) -> None:
    core_mod = _find_bundle_module("core")
    cli_mod = _find_bundle_module("cli")
    if core_mod is None or cli_mod is None:
        raise RuntimeError(
            "core and cli bundles must be installed; "
            "run `uv sync` to install workspace members"
        )
    k.ensure(core_mod.split(".")[0], core_mod, singleton=True, display_name="core")
    k.ensure(cli_mod.split(".")[0], cli_mod, singleton=True, display_name="cli")
    if not boot_all:
        return
    for a in k.list():
        try:
            await k.send(a["id"], {"type": "boot"})
        except Exception as e:
            print(f"  [kernel] boot {a['id']!r} raised: {e}")
