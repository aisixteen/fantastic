"""web_rest surface integration test — both kernels.

Proves the composable route-provider path end-to-end: a `web` host
with a `web_rest` child, at daemon boot, pulls the child's `get_routes`
and mounts `POST /<rest>/<target>` (verb in BODY) + `GET
/<rest>/_reflect`. Canonical Python shape — identical on both kernels.
"""

from __future__ import annotations

import httpx
import pytest

from helpers.seeding import seed_web, seed_web_rest


async def _rest_roundtrip(binary, kernel_factory, parity_tmp, free_port, tag):
    base = parity_tmp(f"web_rest_{tag}")
    workdir = base / "W"
    workdir.mkdir(parents=True)
    port = free_port()

    seed_web(binary, workdir, port)
    rest_id = seed_web_rest(binary, workdir)

    await kernel_factory(workdir, port)

    async with httpx.AsyncClient(timeout=5.0) as client:
        # POST /<rest>/kernel  body={type:list_agents}  (verb in body)
        # "kernel" is a universal alias resolving to the root for dispatch
        # verbs on all three runtimes (python root is `fs_loader`, not `core`).
        r = await client.post(
            f"http://127.0.0.1:{port}/{rest_id}/kernel",
            json={"type": "list_agents"},
        )
        assert r.status_code == 200, f"POST status {r.status_code}: {r.text}"
        data = r.json()
        ids = {a.get("id") for a in data.get("agents", []) if isinstance(a, dict)}
        assert "web" in ids, f"list_agents missing web id: {ids}"

        # GET /<rest>/_reflect/core → that agent's reflect (per-agent).
        r2 = await client.get(f"http://127.0.0.1:{port}/{rest_id}/_reflect/core")
        assert r2.status_code == 200, f"GET status {r2.status_code}"
        assert isinstance(r2.json(), dict)


@pytest.mark.asyncio
async def test_web_rest_python(python_binary, python_kernel, parity_tmp, free_port):
    await _rest_roundtrip(python_binary, python_kernel, parity_tmp, free_port, "py")


@pytest.mark.asyncio
async def test_web_rest_swift(swift_binary, swift_kernel, parity_tmp, free_port):
    await _rest_roundtrip(swift_binary, swift_kernel, parity_tmp, free_port, "sw")
