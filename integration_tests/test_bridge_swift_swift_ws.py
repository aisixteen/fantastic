"""Bridge integration test — Swift ↔ Swift over WS (asymmetric).

Both kernels are Swift. A's `kernel_bridge` opens a WS to B's native
WS server and ships raw `{type:"call", target, payload}` frames;
B's `WebSocket.handleCall` dispatches `kernel.send(target, payload)`
and replies over the same socket. No B-side bridge.

Completes the Python×Swift directed-pair matrix:
  py→py, py→swift, swift→py (other files) + swift→swift (here).

This is also the closest proxy for the Apple app, which embeds the
same `FantasticKernelBridge` + `FantasticWeb` code as the Swift CLI
(via FantasticKernelEmbedded) and is not separately spawnable here.
"""

from __future__ import annotations

import pytest

from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws


@pytest.mark.asyncio
async def test_swift_swift_ws_forward_reflect(
    swift_binary,
    swift_kernel,
    parity_tmp,
    free_port,
):
    base = parity_tmp("sw_sw_ws_reflect")
    workdir_a = base / "A_swift"
    workdir_b = base / "B_swift"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # Swift A (client): web + web_ws (orchestrator drives A over WS) + bridge.
    seed_web(swift_binary, workdir_a, port_a)
    seed_web_ws(swift_binary, workdir_a)
    seed_bridge_ws(
        swift_binary, workdir_a,
        agent_id="bridge",
        peer_id="core",
        peer_port=port_b,
    )

    # Swift B (server): web + web_ws (bridge dials B's /<peer_id>/ws).
    seed_web(swift_binary, workdir_b, port_b)
    seed_web_ws(swift_binary, workdir_b)

    # B (server) up first, then A (client). A's daemon boots the
    # bridge → connects to B.
    kernel_b = await swift_kernel(workdir_b, port_b)
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
    assert (
        "id" in reply and "tree" in reply
    ), f"reply lacks uniform reflect fields: {list(reply.keys())}"
