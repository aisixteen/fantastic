"""Bridge integration test — Swift ↔ Python over WS (asymmetric).

Swift A is the client: its `kernel_bridge` opens a WS to Python B's
`web_ws` and ships raw `{type:"call", target, payload}` frames. B's
`web_ws._on_call` dispatches `kernel.send(target, payload)` and
replies over the same socket. No B-side bridge.

When this passes: Swift's `WebSocketTransport` wire shape is
byte-compatible with Python's canonical `web_ws` server. Wire drift
breaks this test loudly.
"""

from __future__ import annotations

import pytest

from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws


@pytest.mark.asyncio
async def test_swift_python_ws_forward_reflect(
    python_binary,
    swift_binary,
    python_kernel,
    swift_kernel,
    parity_tmp,
    free_port,
):
    base = parity_tmp("sw_py_ws_reflect")
    workdir_a = base / "A_swift"
    workdir_b = base / "B_python"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # Swift A: web (native WS, no web_ws child needed) + bridge.
    seed_web(swift_binary, workdir_a, port_a)
    seed_web_ws(swift_binary, workdir_a)  # WS is opt-in: host needs a web_ws child
    seed_bridge_ws(
        swift_binary, workdir_a,
        agent_id="bridge",
        peer_id="core",
        peer_port=port_b,
    )

    # Python B: web + web_ws (serves /<id>/ws).
    seed_web(python_binary, workdir_b, port_b)
    seed_web_ws(python_binary, workdir_b)

    # B (server) up first, then A (client). A's daemon boots the
    # bridge → connects to B.
    kernel_b = await python_kernel(workdir_b, port_b)
    kernel_a = await swift_kernel(workdir_a, port_a)

    # Idempotent connect guard.
    await kernel_a.call("bridge", "boot")

    reply = await kernel_a.call(
        "bridge", "forward",
        target="core",
        payload={"type": "reflect"},
    )

    assert isinstance(reply, dict), f"expected dict, got {type(reply)}: {reply}"
    assert "error" not in reply, f"forward returned error: {reply}"
    # Uniform reflect carries id + sentence + tree (default tree=all).
    assert "id" in reply and "tree" in reply, (
        f"reply lacks uniform reflect fields: {list(reply.keys())}"
    )
