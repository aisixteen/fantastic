"""Decoupling guard — the rust + python hosts no longer register the
view/webapp bundles that moved to the `ts/` frontend.

After "part 1 — big decoupling" every host kernel is pure: UI lives in
`ts/`, served generically by a `file` agent (weak binding). This test
reads the bundle catalog (`reflect bundles=all`) with an operator one-shot on
each host binary (the root's catalog is the operator's to read: a leg holds no
line on the root), boots the daemon over the same workdir, and asserts none of
the 7 removed view modules are still registered.

Requires the built binaries; skips cleanly without them (the `*_binary`
fixtures call `pytest.skip`). NOT run by the unit-test gate. Run explicitly:

    uv run pytest decoupling/test_decoupling_bundle_catalog.py
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Callable

import pytest

from helpers.seeding import seed_reflect, seed_web

# The 7 view/webapp bundles deleted from every host (UI moved to `ts/`).
REMOVED = {
    "ai_chat_webapp.tools",
    "canvas_backend.tools",
    "canvas_webapp.tools",
    "gl_agent.tools",
    "html_agent.tools",
    "telemetry_pane.tools",
    "terminal_webapp.tools",
}


async def _catalog_has_no_views(
    binary: Path,
    spawn: Callable[..., Any],
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
    tag: str,
) -> None:
    workdir = parity_tmp(tag) / "host"
    workdir.mkdir(parents=True)
    port = free_port()
    seed_web(binary, workdir, port)
    # the operator's read, before the daemon takes the lock
    reflect = seed_reflect(binary, workdir, "kernel", bundles="all")
    await spawn(workdir, port)
    bundles = reflect.get("bundles", [])
    modules = {b.get("handler_module") for b in bundles if isinstance(b, dict)}

    leaked = REMOVED & modules
    assert not leaked, f"{tag}: removed view bundles still registered: {sorted(leaked)}"
    # sanity — the host still exposes its core surface
    assert "file_bridge.tools" in modules, (
        f"{tag}: file_bridge.tools missing from catalog: {sorted(modules)}"
    )


@pytest.mark.asyncio
async def test_rust_catalog_drops_views(rust_binary, rust_kernel, parity_tmp, free_port):
    await _catalog_has_no_views(rust_binary, rust_kernel, parity_tmp, free_port, "rust_decouple")


@pytest.mark.skip(reason="#170: Python product keeps the old grant model")
@pytest.mark.asyncio
async def test_python_catalog_drops_views(python_binary, python_kernel, parity_tmp, free_port):
    # The 'kernel' alias passed to ws_call resolves to the root agent on every
    # runtime regardless of its literal id — python's root is `kernel_state`,
    # rust uses `core`. The same helper covers all three runtimes because
    # dispatch goes through the alias, never a hardcoded literal id.
    await _catalog_has_no_views(
        python_binary, python_kernel, parity_tmp, free_port, "python_decouple"
    )
