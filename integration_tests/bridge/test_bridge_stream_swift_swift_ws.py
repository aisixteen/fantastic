"""Bridge streaming integration test — Swift ↔ Swift over WS.

Both kernels are Swift. Completes the streaming matrix (py→py, py→swift,
swift→py in sibling files; swift→swift here). Also the closest proxy for the
Apple app, which embeds the same Swift bridge + WebSocket code paths end-to-end.

Flow exercised:

  1. A.bridge connects to B's web_ws (asymmetric — no B-side bridge).
  2. A.bridge.watch_remote(target=server_root) → sends {type:"watch",
     src:<server_root>} to B over the WS. B's WebSocket handler registers
     kernel.watch(<server_root>, ...).
  3. An emit on B's root ({type:"emit", target:<server_root>, payload:{...}})
     fans out to B's watcher → B sends {type:"event", payload} back over the
     bridge WS socket.
  4. A.bridge's read loop re-emits the payload on its OWN inbox.
  5. A test client watching A.bridge's inbox sees the forwarded event.

The probe payload carries a unique nonce to distinguish it from boot/handshake
noise (bridge_up frames, the watch_remote call's own fanout, etc.).

Root-id note: both kernels are Swift so server_root will be 'core', but we
resolve it via root_id() rather than hardcoding — uniform with the asymmetric
siblings (swift→py, py→swift) where the server root differs by runtime.
"""

from __future__ import annotations

import os as _os

import pytest

from helpers.seeding import root_id, seed_bridge_ws, seed_web, seed_web_ws
from helpers.streaming import assert_watch_remote_streams

# Local-loopback (127.0.0.1) bridge addressing — meaningless INSIDE a container
# (that's the container's own loopback, not the host). The cross-CONTAINER bridge
# is covered by test_bridge_container_to_container (host.containers.internal +
# all-interface publish), so skip this local matrix under the container target.
pytestmark = pytest.mark.skipif(
    _os.environ.get("FANTASTIC_TARGET", "local").strip().lower() == "container",
    reason="local-loopback bridge; container path = test_bridge_container_to_container",
)


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

    # Swift B (server): web + web_ws (bridge dials it; ws_emit triggers the watch fanout).
    seed_web(swift_binary, workdir_b, port_b)
    seed_web_ws(swift_binary, workdir_b)

    # B is the watched/emitted kernel — resolve its LITERAL root id via reflect.
    # For Swift this will be 'core', but we use root_id() for uniformity with
    # the asymmetric siblings so a runtime change never requires a hardcoded fix here.
    server_root = root_id(swift_binary, workdir_b)

    # Swift A (client): web + web_ws (orchestrator WS surface) + bridge to B.
    seed_web(swift_binary, workdir_a, port_a)
    seed_web_ws(swift_binary, workdir_a)
    seed_bridge_ws(
        swift_binary,
        workdir_a,
        agent_id="bridge",
        peer_id=server_root,
        peer_port=port_b,
    )

    # Spawn B first so it is accepting connections when A's bridge dials on boot.
    await swift_kernel(workdir_b, port_b)  # fixture keeps B alive; not addressed directly
    kernel_a = await swift_kernel(workdir_a, port_a)
    await kernel_a.call("bridge", "boot")

    await assert_watch_remote_streams(port_a, port_b, server_root=server_root)
