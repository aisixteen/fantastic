"""Bridge integration test — Swift A (client) → Python B (server) over WS.

Proves that Swift's ``WebSocketTransport`` wire shape is byte-compatible with
Python's canonical ``web_ws`` server:

- Swift A seeds a ``kernel_bridge`` (transport=ws) that dials B's ``web_ws``
  endpoint on boot.
- Python B runs ``web_ws``, which dispatches incoming ``{type:"call", target,
  payload}`` frames via ``kernel.send(target, payload)`` and replies over the
  same socket.  No bridge agent on B — it is a pure server.
- A's bridge ``forward`` verb relays the payload to B, which dispatches it to
  B's ``kernel`` alias (→ B's actual root, ``kernel_state`` on Python).
- The test verifies that the reflect reply carries the uniform reflect fields
  (``id``, ``sentence``, ``tree``), confirming end-to-end dispatch.

Wire drift — mismatched frame shapes between runtimes — breaks this test
loudly.  Both-binary skip ensures the test is always a no-op when either
binary is absent (never a failure).
"""

from __future__ import annotations

import os as _os
from pathlib import Path

import pytest

from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws

# Local-loopback (127.0.0.1) bridge addressing — meaningless INSIDE a container
# (that's the container's own loopback, not the host). The cross-CONTAINER bridge
# is covered by test_bridge_container_to_container (host.containers.internal +
# all-interface publish), so skip this local matrix under the container target.
pytestmark = pytest.mark.skipif(
    _os.environ.get("FANTASTIC_TARGET", "local").strip().lower() == "container",
    reason="local-loopback bridge; container path = test_bridge_container_to_container",
)


async def test_swift_python_ws_forward_reflect(
    python_binary: Path,
    swift_binary: Path,
    python_kernel,
    swift_kernel,
    parity_tmp,
    free_port,
) -> None:
    """Swift A forwards a ``reflect`` call through its WS bridge to Python B.

    B's root id is ``kernel_state`` (Python runtime); both ``peer_id`` and
    ``target`` use the ``kernel`` alias so the test is runtime-agnostic and
    would survive a future root-id rename.
    """
    base = parity_tmp("sw_py_ws_reflect")
    workdir_a = base / "A_swift"
    workdir_b = base / "B_python"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # Swift A: web + web_ws (WS is opt-in; the host exposes /<id>/ws only when
    # a web_ws child is present) + a bridge agent that dials B on boot.
    seed_web(swift_binary, workdir_a, port_a)
    seed_web_ws(swift_binary, workdir_a)
    seed_bridge_ws(
        swift_binary,
        workdir_a,
        agent_id="bridge",
        # "kernel" is the runtime-neutral alias for the root agent on any kernel
        # (Python root=kernel_state, Swift/Rust root=core).  Using the alias here
        # means the WS path the bridge dials — /<peer_id>/ws — resolves
        # correctly on both runtimes without hardcoding a runtime-specific id.
        peer_id="kernel",
        peer_port=port_b,
    )

    # Python B: web + web_ws (serves /<id>/ws that the bridge connects to).
    # No bridge on B — it is purely a server.
    seed_web(python_binary, workdir_b, port_b)
    seed_web_ws(python_binary, workdir_b)

    # Spawn B first so its web_ws is accepting connections before A's bridge
    # daemon attempts to dial.
    kernel_b = await python_kernel(workdir_b, port_b)
    kernel_a = await swift_kernel(workdir_a, port_a)

    # Explicit boot call is idempotent; ensures the bridge connection is
    # established before we exercise the forward path.
    await kernel_a.call("bridge", "boot")

    # Forward a reflect call through the bridge to B's kernel alias.
    # B dispatches it to its root (kernel_state), which returns the uniform
    # reflect payload.
    reply = await kernel_a.call(
        "bridge",
        "forward",
        target="kernel",
        payload={"type": "reflect"},
    )

    assert isinstance(reply, dict), f"expected dict, got {type(reply)}: {reply}"
    assert "error" not in reply, f"forward returned error: {reply}"

    # Uniform reflect always carries: id (root agent id), sentence (one-line
    # description), tree (agent tree, default=all).
    missing = [f for f in ("id", "sentence", "tree") if f not in reply]
    assert not missing, f"reflect reply missing fields {missing}; got keys: {list(reply.keys())}"

    # Sanity: B's root id is "kernel_state" on Python (not "core").
    # Validate only that we got a non-empty string — the alias resolved correctly.
    assert isinstance(reply["id"], str) and reply["id"], (
        f"reflect 'id' should be a non-empty string, got: {reply['id']!r}"
    )

    # Suppress unused-variable warning: kernel_b is held to keep the process
    # alive for the duration of the test (the fixture terminates it on teardown).
    assert kernel_b is not None
