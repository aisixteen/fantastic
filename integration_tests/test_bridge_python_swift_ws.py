"""Bridge integration test — Python ↔ Swift over WS (asymmetric).

Python A is the client: its `kernel_bridge` opens a WS to Swift B's
native WS server and ships raw `{type:"call", target, payload}`
frames. Swift's `WebSocket.handleCall` dispatches
`kernel.send(target, payload)` and replies over the same socket. No
B-side bridge.

When this passes: Python's canonical wire shape is accepted by
Swift's WS server — the reverse direction of
`test_bridge_swift_python_ws`. Together they prove the wire is
symmetric across runtimes regardless of which side initiates.
"""

from __future__ import annotations

import pytest

from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws


@pytest.mark.asyncio
async def test_python_swift_ws_forward_reflect(
    python_binary,
    swift_binary,
    python_kernel,
    swift_kernel,
    parity_tmp,
    free_port,
):
    base = parity_tmp("py_sw_ws_reflect")
    workdir_a = base / "A_python"
    workdir_b = base / "B_swift"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # Python A: web + web_ws (so the orchestrator can drive it) + bridge.
    seed_web(python_binary, workdir_a, port_a)
    seed_web_ws(python_binary, workdir_a)
    seed_bridge_ws(
        python_binary, workdir_a,
        agent_id="bridge",
        peer_id="core",
        peer_port=port_b,
    )

    # Swift B: web + web_ws (WS is opt-in via the web_ws child).
    seed_web(swift_binary, workdir_b, port_b)
    seed_web_ws(swift_binary, workdir_b)

    # B (server) up first, then A (client).
    kernel_b = await swift_kernel(workdir_b, port_b)
    kernel_a = await python_kernel(workdir_a, port_a)

    await kernel_a.call("bridge", "boot")

    reply = await kernel_a.call(
        "bridge", "forward",
        target="core",
        payload={"type": "reflect"},
    )

    assert isinstance(reply, dict), f"expected dict, got {type(reply)}: {reply}"
    assert "error" not in reply, f"forward returned error: {reply}"
    # Uniform reflect carries id + sentence + tree (default tree=all).
    assert (
        "id" in reply and "tree" in reply
    ), f"reply lacks uniform reflect fields: {list(reply.keys())}"
