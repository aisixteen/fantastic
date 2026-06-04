"""web_rest surface integration test — python + swift kernels.

Proves the composable route-provider path end-to-end: a `web` host
with a `web_rest` child, at daemon boot, pulls the child's `get_routes`
and mounts:
  - POST /<rest>/<target>   verb carried in the JSON body
  - GET  /<rest>/_reflect/<agent_id>   per-agent reflect

Two assertions per runtime:
1. POST /<rest>/kernel {type:list_agents} returns agents that include
   the seeded `web` agent. The `kernel` alias resolves to the root on
   all runtimes (python root = `fs_loader`; rust/swift root = `core`).
2. GET  /<rest>/_reflect/web returns a dict. We reflect `web` (not
   `core`) because `web` is guaranteed present after seeding on every
   runtime, making the assertion portable across python/swift/rust.
"""

from __future__ import annotations

import httpx
import pytest

from helpers.seeding import seed_web, seed_web_rest


async def _rest_roundtrip(binary, kernel_factory, parity_tmp, free_port, tag: str) -> None:
    base = parity_tmp(f"web_rest_{tag}")
    workdir = base / "W"
    workdir.mkdir(parents=True)
    port = free_port()

    seed_web(binary, workdir, port)
    rest_id = seed_web_rest(binary, workdir)

    await kernel_factory(workdir, port)

    async with httpx.AsyncClient(timeout=5.0) as client:
        # POST /<rest>/kernel  body={type:list_agents}  (verb in body).
        # "kernel" is a universal dispatch alias that resolves to the root
        # agent on all three runtimes (python root is `fs_loader`, not `core`).
        r = await client.post(
            f"http://127.0.0.1:{port}/{rest_id}/kernel",
            json={"type": "list_agents"},
        )
        assert r.status_code == 200, f"POST list_agents status {r.status_code}: {r.text}"
        data = r.json()
        ids = {a.get("id") for a in data.get("agents", []) if isinstance(a, dict)}
        assert "web" in ids, f"list_agents missing 'web' agent: {ids}"

        # GET /<rest>/_reflect/web → the `web` agent's reflect payload.
        # Using `web` (seeded on every runtime) rather than `core` (alias
        # that varies by runtime) keeps this assertion portable.
        r2 = await client.get(f"http://127.0.0.1:{port}/{rest_id}/_reflect/web")
        assert r2.status_code == 200, f"GET _reflect/web status {r2.status_code}: {r2.text}"
        assert isinstance(r2.json(), dict), f"_reflect/web did not return a dict: {r2.text!r}"


@pytest.mark.asyncio
async def test_web_rest_python(python_binary, python_kernel, parity_tmp, free_port):
    await _rest_roundtrip(python_binary, python_kernel, parity_tmp, free_port, "py")


@pytest.mark.asyncio
async def test_web_rest_swift(swift_binary, swift_kernel, parity_tmp, free_port):
    await _rest_roundtrip(swift_binary, swift_kernel, parity_tmp, free_port, "sw")
