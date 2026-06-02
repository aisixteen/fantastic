"""Bridge integration — Rust in the cross-runtime matrix (WS-only).

Rust is a peer kernel, same as Python/Swift. These exercise the
restored + asymmetric-rewritten rust kernel_bridge against all
runtimes (forward) + rust↔rust streaming (watch_remote):

  rust→rust (same) · rust→python · python→rust · rust→swift · swift→rust

Each: client = web + web_ws + bridge; server = web + web_ws. Server
spawns first (the bridge connects eagerly); then `boot` is an
idempotent connect guard.
"""

from __future__ import annotations

import pytest

from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws
from helpers.streaming import assert_watch_remote_streams


async def _forward_reflect(
    client_bin, client_spawn, server_bin, server_spawn, parity_tmp, free_port, tag
):
    base = parity_tmp(tag)
    wa = base / "A_client"
    wb = base / "B_server"
    wa.mkdir(parents=True)
    wb.mkdir(parents=True)
    pa = free_port()
    pb = free_port()

    seed_web(client_bin, wa, pa)
    seed_web_ws(client_bin, wa)
    seed_bridge_ws(client_bin, wa, agent_id="bridge", peer_id="kernel", peer_port=pb)

    seed_web(server_bin, wb, pb)
    seed_web_ws(server_bin, wb)

    await server_spawn(wb, pb)  # server first
    ka = await client_spawn(wa, pa)
    await ka.call("bridge", "boot")

    reply = await ka.call("bridge", "forward", target="kernel", payload={"type": "reflect"})
    assert isinstance(reply, dict), f"expected dict, got {type(reply)}: {reply}"
    assert "error" not in reply, f"forward returned error: {reply}"
    # Uniform reflect carries id + sentence + tree (transports/primitive
    # moved to the readme).
    assert all(
        k in reply for k in ("id", "sentence", "tree")
    ), f"reply lacks uniform reflect fields: {list(reply.keys())}"


@pytest.mark.asyncio
async def test_rust_rust_ws_forward(rust_binary, rust_kernel, parity_tmp, free_port):
    await _forward_reflect(
        rust_binary, rust_kernel, rust_binary, rust_kernel, parity_tmp, free_port,
        "rust_rust",
    )


@pytest.mark.asyncio
async def test_rust_python_ws_forward(
    rust_binary, rust_kernel, python_binary, python_kernel, parity_tmp, free_port
):
    await _forward_reflect(
        rust_binary, rust_kernel, python_binary, python_kernel, parity_tmp, free_port,
        "rust_python",
    )


@pytest.mark.asyncio
async def test_python_rust_ws_forward(
    python_binary, python_kernel, rust_binary, rust_kernel, parity_tmp, free_port
):
    await _forward_reflect(
        python_binary, python_kernel, rust_binary, rust_kernel, parity_tmp, free_port,
        "python_rust",
    )


@pytest.mark.asyncio
async def test_rust_swift_ws_forward(
    rust_binary, rust_kernel, swift_binary, swift_kernel, parity_tmp, free_port
):
    await _forward_reflect(
        rust_binary, rust_kernel, swift_binary, swift_kernel, parity_tmp, free_port,
        "rust_swift",
    )


@pytest.mark.asyncio
async def test_swift_rust_ws_forward(
    swift_binary, swift_kernel, rust_binary, rust_kernel, parity_tmp, free_port
):
    await _forward_reflect(
        swift_binary, swift_kernel, rust_binary, rust_kernel, parity_tmp, free_port,
        "swift_rust",
    )


@pytest.mark.asyncio
async def test_rust_rust_ws_watch_remote_streams_event(
    rust_binary, rust_kernel, parity_tmp, free_port
):
    base = parity_tmp("rust_rust_stream")
    wa = base / "A"
    wb = base / "B"
    wa.mkdir(parents=True)
    wb.mkdir(parents=True)
    pa = free_port()
    pb = free_port()
    for wd, port in [(wa, pa), (wb, pb)]:
        seed_web(rust_binary, wd, port)
        seed_web_ws(rust_binary, wd)
    seed_bridge_ws(rust_binary, wa, agent_id="bridge", peer_id="core", peer_port=pb)

    await rust_kernel(wb, pb)
    ka = await rust_kernel(wa, pa)
    await ka.call("bridge", "boot")

    await assert_watch_remote_streams(pa, pb)
