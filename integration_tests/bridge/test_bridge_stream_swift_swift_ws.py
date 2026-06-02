"""Bridge streaming integration test — Swift ↔ Swift over WS.

Both kernels Swift. Completes the streaming matrix (py→py, swift→py,
py→swift here in sibling files + swift→swift). Also the closest proxy
for the Apple app, which embeds the same Swift bridge + WS code.
"""

from __future__ import annotations

import pytest

from helpers.seeding import root_id, seed_bridge_ws, seed_web, seed_web_ws
from helpers.streaming import assert_watch_remote_streams


@pytest.mark.asyncio
async def test_swift_swift_ws_watch_remote_streams_event(
    swift_binary, swift_kernel, parity_tmp, free_port
):
    base = parity_tmp("sw_sw_ws_stream")
    workdir_a = base / "A_swift"
    workdir_b = base / "B_swift"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # Swift B (server): web + web_ws (bridge dials + ws_emit).
    seed_web(swift_binary, workdir_b, port_b)
    seed_web_ws(swift_binary, workdir_b)

    # B is the watched/emitted kernel — resolve its LITERAL root id.
    server_root = root_id(swift_binary, workdir_b)

    # Swift A (client): web + web_ws (orchestrator + watch over WS) + bridge.
    seed_web(swift_binary, workdir_a, port_a)
    seed_web_ws(swift_binary, workdir_a)
    seed_bridge_ws(
        swift_binary, workdir_a,
        agent_id="bridge",
        peer_id=server_root,
        peer_port=port_b,
    )

    kernel_b = await swift_kernel(workdir_b, port_b)
    kernel_a = await swift_kernel(workdir_a, port_a)
    await kernel_a.call("bridge", "boot")

    await assert_watch_remote_streams(port_a, port_b, server_root=server_root)
