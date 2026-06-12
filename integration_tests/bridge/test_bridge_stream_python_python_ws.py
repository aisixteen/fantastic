"""Bridge streaming integration test — Python client ↔ Python server over WS.

Exercises the `watch_remote` path end-to-end with two Python kernels:

  1. A.bridge connects to B's web_ws (asymmetric — no bridge agent on B).
  2. A.bridge.watch_remote(target=server_root) → sends {type:"watch",
     src:<server_root>} to B. B's web_ws registers kernel.watch(<server_root>, ...).
     `server_root` is B's LITERAL root id, resolved at test time via
     root_id(python_binary, workdir_b) — for Python this is "kernel_state".
     watch/emit do LITERAL id lookups; the "kernel" dispatch alias does NOT work here.
  3. An emit on B's root ({type:"emit", target:<server_root>, payload:{...}})
     fans out to B's watcher → B sends {type:"event", payload} back
     over the bridge WS.
  4. A.bridge's read loop re-emits payload on its OWN inbox.
  5. A test client watching A.bridge's inbox sees the event.

The probe payload carries a unique nonce so we can distinguish it
from boot/handshake noise (bridge_up, the watch_remote call's own fanout).

This file is the CANONICAL reference for literal-root discipline:
  - seed_bridge_ws(peer_id=server_root) — peer WS path uses the literal root
  - assert_watch_remote_streams(server_root=server_root) — watch/emit use literal root
  - root_id() is called once and reused, never hardcoded as "core" or "kernel_state"
"""

from __future__ import annotations

import pytest

from helpers.seeding import root_id, seed_bridge_ws, seed_web, seed_web_ws
from helpers.streaming import assert_watch_remote_streams

import os as _os

# Local-loopback (127.0.0.1) bridge addressing — meaningless inside a container.
# The cross-container bridge is covered by test_bridge_container_to_container
# (host.containers.internal); skip this local matrix under the container target.
pytestmark = pytest.mark.skipif(
    _os.environ.get("FANTASTIC_TARGET", "local").strip().lower() == "container",
    reason="local-loopback bridge; container path = test_bridge_container_to_container",
)


@pytest.mark.asyncio
async def test_python_python_ws_watch_remote_streams_event(
    python_binary, python_kernel, parity_tmp, free_port
):
    base = parity_tmp("py_py_ws_stream")
    workdir_a = base / "A"
    workdir_b = base / "B"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # B is the watched/emitted kernel — resolve its LITERAL root id before seeding.
    # Python's root is "kernel_state"; rust/swift use "core". Never hardcode either.
    server_root = root_id(python_binary, workdir_b)

    # Seed B (server) first: web HTTP listener + web_ws WS route (opt-in).
    seed_web(python_binary, workdir_b, port_b)
    seed_web_ws(python_binary, workdir_b)

    # Seed A (client): web + web_ws (drivable) + bridge pointing at B's literal root.
    seed_web(python_binary, workdir_a, port_a)
    seed_web_ws(python_binary, workdir_a)
    seed_bridge_ws(
        python_binary,
        workdir_a,
        agent_id="bridge",
        peer_id=server_root,  # selects ws://<host>:<port_b>/<server_root>/ws on B
        peer_port=port_b,
    )

    # Spawn B before A so B's WS surface is ready when A's bridge dials it at boot.
    await python_kernel(workdir_b, port_b)  # fixture keeps B alive; not addressed directly
    kernel_a = await python_kernel(workdir_a, port_a)
    await kernel_a.call("bridge", "boot")

    await assert_watch_remote_streams(port_a, port_b, server_root=server_root)
