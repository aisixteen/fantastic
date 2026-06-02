"""Decoupled-frontend proof — a host with NO view bundles still serves a
static UI generically through a `file` agent.

This is the post-decoupling contract: the host renders no HTML itself;
the `ts/` frontend (or any view package) is served from a `file` agent
rooted at its built `dist/`, reachable at `GET /<id>/file/<path>`. Here
we root a `file` agent at a throwaway dir holding an `index.html` and
fetch it over HTTP — the same recipe that serves `ts/dist`, exercised on
the real rust + swift binaries.

GENERATED scaffold — needs the built rust/swift binaries; skips cleanly
without them. NOT run by the unit-test gate. Run explicitly:

    cd integration_tests && uv run pytest test_serve_frontend.py
"""

from __future__ import annotations

import httpx
import pytest

from helpers.seeding import seed_create, seed_web

_MARKER = "FRONTEND-OK"


async def _serves_static_via_file_agent(binary, spawn, parity_tmp, free_port, tag):
    workdir = parity_tmp(tag) / "host"
    workdir.mkdir(parents=True)
    # The "dist" a real frontend would point at — a plain static dir.
    dist = workdir / "frontend_dist"
    dist.mkdir()
    (dist / "index.html").write_text(f"<h1>{_MARKER}</h1>", encoding="utf-8")

    port = free_port()
    seed_web(binary, workdir, port)
    # Generic static host: a `file` agent rooted at the dist dir. No view
    # bundle involved — exactly how `ts_dist` serves `ts/dist`.
    seed_create(binary, workdir, handler_module="file.tools", agent_id="assets", root=str(dist))
    await spawn(workdir, port)

    url = f"http://127.0.0.1:{port}/assets/file/index.html"
    async with httpx.AsyncClient(timeout=10.0) as client:
        resp = await client.get(url)
    assert resp.status_code == 200, f"{tag}: GET {url} → {resp.status_code}"
    assert _MARKER in resp.text, f"{tag}: served body missing marker: {resp.text[:200]!r}"


@pytest.mark.asyncio
async def test_rust_serves_frontend_generically(rust_binary, rust_kernel, parity_tmp, free_port):
    await _serves_static_via_file_agent(
        rust_binary, rust_kernel, parity_tmp, free_port, "rust_serve"
    )


@pytest.mark.asyncio
async def test_swift_serves_frontend_generically(swift_binary, swift_kernel, parity_tmp, free_port):
    await _serves_static_via_file_agent(
        swift_binary, swift_kernel, parity_tmp, free_port, "swift_serve"
    )
