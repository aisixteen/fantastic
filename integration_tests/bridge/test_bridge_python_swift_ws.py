"""Bridge integration test — Python A (client) → Swift B (server) over WS.

What is proved here
-------------------
Python's `ws_bridge` (WS transport) can open a connection to Swift's
native WS server and round-trip a `forward` call. The response is a well-formed
uniform-reflect payload (``id`` + ``tree`` keys present), confirming that:

  1. Python's ``{type:"call", target, payload}`` wire shape is accepted by
     Swift's WS dispatcher.
  2. The ``kernel`` *alias* resolves correctly on Swift B (Swift's literal root
     id is ``core``; Python's is ``kernel_state`` — neither is hardcoded here).
  3. The reflect response shape is consistent across runtimes.

This is the directed counterpart of ``test_bridge_swift_python_ws``. Together
the two tests prove the wire is symmetric regardless of which side initiates.

Root-id note: ``peer_id='kernel'`` in ``seed_bridge_ws`` selects the WS path
on Swift B (``/<peer_id>/ws``). The ``target='kernel'`` in the forwarded frame
dispatches to the kernel alias on Swift. Neither Python's ``kernel_state`` nor
Swift's ``core`` is hardcoded — the ``kernel`` alias works on all runtimes.
"""

from __future__ import annotations

import os as _os
from pathlib import Path

import pytest

from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws
from helpers.kernel_proc import KernelProc

# Local-loopback (127.0.0.1) bridge addressing — meaningless INSIDE a container
# (that's the container's own loopback, not the host). The cross-CONTAINER bridge
# is covered by test_bridge_container_to_container (host.containers.internal +
# all-interface publish), so skip this local matrix under the container target.
pytestmark = pytest.mark.skipif(
    _os.environ.get("FANTASTIC_TARGET", "local").strip().lower() == "container",
    reason="local-loopback bridge; container path = test_bridge_container_to_container",
)


@pytest.mark.asyncio
async def test_python_swift_ws_forward_reflect(
    python_binary: Path,
    swift_binary: Path,
    python_kernel,
    swift_kernel,
    parity_tmp,
    free_port,
) -> None:
    """Python A dials Swift B; a forwarded reflect must return id + tree."""
    base = parity_tmp("py_sw_ws_reflect")
    workdir_a: Path = base / "A_python"
    workdir_b: Path = base / "B_swift"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a: int = free_port()
    port_b: int = free_port()

    # Seed Python A: web listener + WS upgrade + bridge that dials Swift B.
    # peer_id='kernel' selects the WS path on B (/<peer_id>/ws). The 'kernel'
    # alias resolves to whatever root id B's runtime uses — no hardcoding.
    seed_web(python_binary, workdir_a, port_a)
    seed_web_ws(python_binary, workdir_a)
    seed_bridge_ws(
        python_binary,
        workdir_a,
        agent_id="bridge",
        peer_id="kernel",
        peer_port=port_b,
    )

    # Seed Swift B: web listener + WS upgrade. No bridge on the server side —
    # the asymmetric design requires only the client to hold the bridge agent.
    seed_web(swift_binary, workdir_b, port_b)
    seed_web_ws(swift_binary, workdir_b)

    # B (server) must be up before A (client) connects — spawn order matters.
    # The fixture keeps B alive for the test's duration; we don't address it directly.
    await swift_kernel(workdir_b, port_b)
    kernel_a: KernelProc = await python_kernel(workdir_a, port_a)

    # Boot the bridge (idempotent — re-running is safe; ensures the WS dial
    # completes before we issue the first forward).
    await kernel_a.call("bridge", "boot")

    # Forward a reflect to the 'kernel' alias on Swift B and validate the
    # uniform-reflect response shape.
    reply: dict = await kernel_a.call(
        "bridge",
        "forward",
        target="kernel",
        payload={"type": "reflect"},
    )

    assert isinstance(reply, dict), f"expected dict, got {type(reply)}: {reply}"
    assert "error" not in reply, f"bridge forward returned error: {reply}"

    # Uniform reflect must carry at minimum 'id' and 'tree'.
    missing = [key for key in ("id", "tree") if key not in reply]
    assert not missing, (
        f"reflect reply missing required fields {missing}; got keys: {list(reply.keys())}"
    )

    # Sanity: the reflected id should be a non-empty string.
    assert isinstance(reply["id"], str) and reply["id"], (
        f"reflect 'id' should be a non-empty string, got: {reply['id']!r}"
    )
