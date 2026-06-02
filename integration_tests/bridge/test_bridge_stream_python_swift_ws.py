"""Bridge streaming integration test — Python client → Swift server.

Python A's bridge subscribes to Swift B's core via `watch_remote`.
Exercises: Python `kernel_bridge._watch_remote` (sends {watch,src})
→ Swift `WebSocket.handleFrame` "watch" path → emit on B.core →
{event} back over the WS → Python A's read loop re-emits on the
bridge inbox. This is the path Phase 4 (Swift explicit-watch server)
unlocked.
"""

from __future__ import annotations

import pytest

from helpers.seeding import root_id, seed_bridge_ws, seed_web, seed_web_ws
from helpers.streaming import assert_watch_remote_streams


@pytest.mark.asyncio
async def test_python_swift_ws_watch_remote_streams_event(
    python_binary, swift_binary, python_kernel, swift_kernel, parity_tmp, free_port
):
    base = parity_tmp("py_sw_ws_stream")
    workdir_a = base / "A_python"
    workdir_b = base / "B_swift"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # Swift B (server): web + web_ws (WS opt-in; bridge dials + ws_emit).
    seed_web(swift_binary, workdir_b, port_b)
    seed_web_ws(swift_binary, workdir_b)

    # B is the watched/emitted kernel — resolve its LITERAL root id.
    server_root = root_id(swift_binary, workdir_b)

    # Python A (client): web + web_ws (drivable) + bridge.
    seed_web(python_binary, workdir_a, port_a)
    seed_web_ws(python_binary, workdir_a)
    seed_bridge_ws(
        python_binary, workdir_a,
        agent_id="bridge",
        peer_id=server_root,
        peer_port=port_b,
    )

    kernel_b = await swift_kernel(workdir_b, port_b)
    kernel_a = await python_kernel(workdir_a, port_a)
    await kernel_a.call("bridge", "boot")

    await assert_watch_remote_streams(port_a, port_b, server_root=server_root)
