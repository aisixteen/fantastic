"""Bridge streaming integration test — Swift client → Python server.

Swift A's bridge subscribes to Python B's core via `watch_remote`.
Exercises: Swift `WebSocketTransport.watchRemote` (sends {watch,src})
→ Python `web_ws._on_watch` → emit on B.core → {event} back over the
WS → Swift A's read loop re-emits on the bridge inbox.
"""

from __future__ import annotations

import pytest

from helpers.seeding import root_id, seed_bridge_ws, seed_web, seed_web_ws
from helpers.streaming import assert_watch_remote_streams


@pytest.mark.asyncio
async def test_swift_python_ws_watch_remote_streams_event(
    python_binary, swift_binary, python_kernel, swift_kernel, parity_tmp, free_port
):
    base = parity_tmp("sw_py_ws_stream")
    workdir_a = base / "A_swift"
    workdir_b = base / "B_python"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # Python B (server): web + web_ws.
    seed_web(python_binary, workdir_b, port_b)
    seed_web_ws(python_binary, workdir_b)

    # B is the watched/emitted kernel — resolve its LITERAL root id.
    server_root = root_id(python_binary, workdir_b)

    # Swift A (client): web (native WS) + bridge.
    seed_web(swift_binary, workdir_a, port_a)
    seed_web_ws(swift_binary, workdir_a)  # WS opt-in (orchestrator + watch)
    seed_bridge_ws(
        swift_binary, workdir_a,
        agent_id="bridge",
        peer_id=server_root,
        peer_port=port_b,
    )

    kernel_b = await python_kernel(workdir_b, port_b)
    kernel_a = await swift_kernel(workdir_a, port_a)
    await kernel_a.call("bridge", "boot")

    await assert_watch_remote_streams(port_a, port_b, server_root=server_root)
