"""Bridge streaming integration test — Python client → Swift server.

Exercises the `watch_remote` path end-to-end across runtimes:

  1. A.bridge (Python) connects to B's web_ws (Swift, asymmetric).
  2. A.bridge.watch_remote(target=server_root) → sends {type:"watch",
     src:<server_root>} to B. B's WS server registers a watch on its
     root agent.
  3. An emit on B's root ({type:"emit", target:<server_root>, payload:{}})
     fans out to B's watcher → B sends {type:"event", payload} back
     over the bridge WS.
  4. A.bridge's read loop re-emits the payload on its OWN inbox.
  5. A test client watching A.bridge's inbox sees the event.

`server_root` is resolved via `root_id(swift_binary, workdir_b)` (= 'core'
for Swift) rather than hardcoded. Both `seed_bridge_ws(peer_id=...)` and
`assert_watch_remote_streams(server_root=...)` receive the resolved literal
id because `watch`/`emit` do a literal id lookup — the 'kernel' alias is
only valid for `call` dispatch, not watch/emit paths.

The probe payload carries a unique nonce so we distinguish it from
boot/handshake noise (bridge_up, the watch_remote call's own fanout).
This is the path Phase 4 (Swift explicit-watch server) unlocked.
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

    port_a: int = free_port()
    port_b: int = free_port()

    # Swift B (server): web + web_ws (WS opt-in; bridge dials it, ws_emit probes it).
    seed_web(swift_binary, workdir_b, port_b)
    seed_web_ws(swift_binary, workdir_b)

    # B is the watched/emitted kernel — resolve its LITERAL root id.
    # Swift root = 'core'; Python root = 'fs_loader'. Never hardcode either.
    server_root: str = root_id(swift_binary, workdir_b)

    # Python A (client): web + web_ws (drivable via WS) + bridge to B.
    seed_web(python_binary, workdir_a, port_a)
    seed_web_ws(python_binary, workdir_a)
    seed_bridge_ws(
        python_binary,
        workdir_a,
        agent_id="bridge",
        peer_id=server_root,  # WS path on B: ws://<host>:<port_b>/<server_root>/ws
        peer_port=port_b,
    )

    # Spawn B first so its WS surface is ready before A's bridge dials.
    await swift_kernel(workdir_b, port_b)  # fixture keeps B alive; not addressed directly
    kernel_a = await python_kernel(workdir_a, port_a)
    await kernel_a.call("bridge", "boot")

    await assert_watch_remote_streams(port_a, port_b, server_root=server_root)
