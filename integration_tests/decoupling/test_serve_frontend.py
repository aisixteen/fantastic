"""Decoupled-frontend proof — a host with NO view bundles serves the REAL
sovereign frontend artifact (`ts/dist/js_kernel.zip`) generically through a
`file` agent, with the served bytes' integrity verified against the sha256
digest embedded in the artifact's own `readme.md`.

What is proven:
- A `file` agent rooted at a plain directory serves `bundle.min.js` over HTTP.
- The bytes delivered by the kernel match the artifact's own integrity line,
  confirming zero corruption end-to-end.
- Individual zip members are pulled directly (no full unzip, no unpacked tree),
  consistent with the direct-pull discipline in the rest of the harness.
- The contract holds on all three host runtimes: Python, Rust, and Swift.

GENERATED scaffold — needs the built runtime binaries AND the frontend artifact;
each skips cleanly when its prerequisite is absent. NOT run by the unit-test
gate. Run explicitly:

    cd integration_tests && uv run pytest decoupling/test_serve_frontend.py
"""

from __future__ import annotations

import hashlib

import httpx
import pytest

from helpers.seeding import (
    expected_bundle_sha,
    frontend_zip,
    pull_member_from_zip,
    seed_create,
    seed_web,
)


async def _serves_frontend_bundle_via_file_bridge(
    binary, spawn, parity_tmp, free_port, tag: str
) -> None:
    # Skip guard: require the built frontend artifact.
    zip_path = frontend_zip()
    if not zip_path.exists():
        pytest.skip(f"frontend artifact not built: {zip_path} (run: cd ts && sh scripts/pack.sh)")

    workdir = parity_tmp(tag) / "host"
    workdir.mkdir(parents=True)
    servedir = workdir / "servedir"
    servedir.mkdir()

    # Direct-pull only the member we serve — no full unzip.
    bundle_bytes = pull_member_from_zip(zip_path, "bundle.min.js", servedir / "bundle.min.js")
    # Scrape expected sha from readme.md inside the zip (also pulled, not extracted).
    expected = expected_bundle_sha(zip_path)

    port = free_port()
    seed_web(binary, workdir, port)
    # Generic file_bridge agent — same recipe as the ts_dist file_bridge agent that serves ts/dist.
    # No view bundle involved: the host is completely view-agnostic.
    seed_create(
        binary,
        workdir,
        handler_module="file_bridge.tools",
        agent_id="js_kernel",
        # RELATIVE root: file_bridge clamps roots inside the running dir (= workdir),
        # and the fs edge seals by default - open it for the /file/ proxy.
        root="servedir",
        ingress_rule="allow_all",
    )
    await spawn(workdir, port)

    url = f"http://127.0.0.1:{port}/js_kernel/file/bundle.min.js"
    # Generous timeout — the bundle is ~1.2 MB.
    async with httpx.AsyncClient(timeout=30.0) as client:
        resp = await client.get(url)

    assert resp.status_code == 200, f"{tag}: GET {url} → {resp.status_code}"

    # Primary assertion: served bytes match the artifact's own integrity digest.
    got = hashlib.sha256(resp.content).hexdigest()
    assert got == expected, f"{tag}: served bundle sha {got} != artifact expected {expected}"
    # Belt-and-suspenders: pulled bytes also match (catches local I/O corruption).
    assert got == hashlib.sha256(bundle_bytes).hexdigest(), (
        f"{tag}: served sha {got} != locally pulled sha"
    )


@pytest.mark.asyncio
async def test_python_serves_frontend_bundle(
    python_binary, python_kernel, parity_tmp, free_port
) -> None:
    await _serves_frontend_bundle_via_file_bridge(
        python_binary, python_kernel, parity_tmp, free_port, "python_serve_frontend"
    )


@pytest.mark.asyncio
async def test_rust_serves_frontend_bundle(rust_binary, rust_kernel, parity_tmp, free_port) -> None:
    await _serves_frontend_bundle_via_file_bridge(
        rust_binary, rust_kernel, parity_tmp, free_port, "rust_serve_frontend"
    )


@pytest.mark.asyncio
async def test_swift_serves_frontend_bundle(
    swift_binary, swift_kernel, parity_tmp, free_port
) -> None:
    await _serves_frontend_bundle_via_file_bridge(
        swift_binary, swift_kernel, parity_tmp, free_port, "swift_serve_frontend"
    )
