"""Decoupling guard — the rust + swift hosts no longer register the
view/webapp bundles that moved to the `ts/` frontend.

After "part 1 — big decoupling" every host kernel is pure: UI lives in
`ts/`, served generically by a `file` agent (weak binding). This test
reflects the live bundle catalog (`reflect bundles=all`) over WS on each
host binary and asserts none of the 7 removed view modules are still
registered.

GENERATED scaffold — needs the built rust/swift binaries; skips cleanly
without them (the `*_binary` fixtures `pytest.skip`). NOT run by the
unit-test gate. Run explicitly:

    cd integration_tests && uv run pytest test_decoupling_bundle_catalog.py
"""

from __future__ import annotations

import pytest

from helpers.seeding import seed_web, seed_web_ws
from helpers.ws import ws_call

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


async def _catalog_has_no_views(binary, spawn, parity_tmp, free_port, tag):
    workdir = parity_tmp(tag) / "host"
    workdir.mkdir(parents=True)
    port = free_port()
    seed_web(binary, workdir, port)
    seed_web_ws(binary, workdir)  # WS route so `reflect` reaches /core/ws
    await spawn(workdir, port)

    reflect = await ws_call(port, "kernel", "reflect", bundles="all")
    bundles = reflect.get("bundles", [])
    modules = {b.get("handler_module") for b in bundles if isinstance(b, dict)}

    leaked = REMOVED & modules
    assert not leaked, f"{tag}: removed view bundles still registered: {sorted(leaked)}"
    # sanity — the host still exposes its core surface
    assert "file.tools" in modules, f"{tag}: file.tools missing from catalog: {sorted(modules)}"


@pytest.mark.asyncio
async def test_rust_catalog_drops_views(rust_binary, rust_kernel, parity_tmp, free_port):
    await _catalog_has_no_views(rust_binary, rust_kernel, parity_tmp, free_port, "rust_decouple")


@pytest.mark.asyncio
async def test_swift_catalog_drops_views(swift_binary, swift_kernel, parity_tmp, free_port):
    await _catalog_has_no_views(swift_binary, swift_kernel, parity_tmp, free_port, "swift_decouple")


@pytest.mark.asyncio
async def test_python_catalog_drops_views(python_binary, python_kernel, parity_tmp, free_port):
    # Canonical reference runtime is guarded too — python's root id is also
    # `core` over WS, matching the `_catalog_has_no_views` helper.
    await _catalog_has_no_views(python_binary, python_kernel, parity_tmp, free_port, "python_decouple")
