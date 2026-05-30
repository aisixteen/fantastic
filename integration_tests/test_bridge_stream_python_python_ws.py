"""Bridge streaming integration test — Python ↔ Python over WS.

Exercises the `watch_remote` path end-to-end:

  1. A.bridge connects to B's web_ws (asymmetric).
  2. A.bridge.watch_remote(target="core") → sends {type:"watch",
     src:"core"} to B. B's web_ws registers kernel.watch("core", ...).
  3. An emit on B's core ({type:"emit", target:"core", payload:{...}})
     fans out to B's watcher → B sends {type:"event", payload} back
     over the bridge WS.
  4. A.bridge's read loop re-emits payload on its OWN inbox.
  5. A test client watching A.bridge's inbox sees the event.

The probe payload carries a unique nonce so we can distinguish it
from boot/handshake noise (bridge_up, the watch_remote call's own
fanout).
"""

from __future__ import annotations

import pytest

from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws
from helpers.streaming import assert_watch_remote_streams


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

    for wd, port in [(workdir_a, port_a), (workdir_b, port_b)]:
        seed_web(python_binary, wd, port)
        seed_web_ws(python_binary, wd)

    seed_bridge_ws(
        python_binary, workdir_a,
        agent_id="bridge",
        peer_id="core",
        peer_port=port_b,
    )

    kernel_b = await python_kernel(workdir_b, port_b)
    kernel_a = await python_kernel(workdir_a, port_a)
    await kernel_a.call("bridge", "boot")

    await assert_watch_remote_streams(port_a, port_b)
