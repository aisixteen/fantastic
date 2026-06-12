"""Bridge integration test — Swift A (client) → Swift B (server) over WS.

Proves that Swift's ``WebSocketTransport`` wire shape is self-consistent
(byte-compatible with the same runtime acting as server):

- Swift A seeds a ``kernel_bridge`` (transport=ws) that dials B's native WS
  server on boot.
- Swift B's native WS handler dispatches incoming ``{type:"call", target,
  payload}`` frames via ``kernel.send(target, payload)`` and replies over the
  same socket.  No bridge agent on B — it is a pure server.
- A's bridge ``forward`` verb relays the payload to B, which dispatches it to
  B's ``kernel`` alias (→ B's root, ``core`` on Swift).
- The test verifies that the reflect reply carries the uniform reflect fields
  (``id``, ``sentence``, ``tree``), confirming end-to-end dispatch.

Both roots are ``core`` here, so there is no root-id asymmetry between A and B.
We still use the ``kernel`` alias throughout (``peer_id="kernel"``,
``target="kernel"``) for uniformity with the rest of the cross-runtime matrix —
do NOT hardcode ``"core"``.

This is also the closest integration-test proxy for the Apple app, which embeds
the same ``FantasticKernelBridge`` + ``FantasticWeb`` code as the Swift CLI via
``FantasticKernelEmbedded`` and is not separately spawnable here.

Completes the Python×Swift directed-pair matrix:
  py→py, py→swift, swift→py (other files) + swift→swift (here).

Both-binary skip ensures the test is always a no-op when the swift binary is
absent (never a failure).
"""

from __future__ import annotations

from pathlib import Path


from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws


async def test_swift_swift_ws_forward_reflect(
    swift_binary: Path,
    swift_kernel,
    parity_tmp,
    free_port,
) -> None:
    """Swift A forwards a ``reflect`` call through its WS bridge to Swift B.

    Both roots are ``core``; the ``kernel`` alias resolves to ``core`` on each
    kernel.  The forward must cross the WS bridge and return a valid uniform
    reflect payload with ``id``, ``sentence``, and ``tree``.
    """
    base = parity_tmp("sw_sw_ws_reflect")
    workdir_a = base / "A_swift"
    workdir_b = base / "B_swift"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # Swift A (client): web + web_ws (WS is opt-in; the host exposes /<id>/ws
    # only when a web_ws child is present) + a bridge agent that dials B on boot.
    seed_web(swift_binary, workdir_a, port_a)
    seed_web_ws(swift_binary, workdir_a)
    seed_bridge_ws(
        swift_binary,
        workdir_a,
        agent_id="bridge",
        # "kernel" is the runtime-neutral alias for the root agent on any kernel
        # (Python root=kernel_state, Swift/Rust root=core).  Using the alias here
        # means the WS path the bridge dials — /<peer_id>/ws — resolves
        # correctly on all runtimes without hardcoding a runtime-specific id.
        peer_id="kernel",
        peer_port=port_b,
    )

    # Swift B (server): web + web_ws (serves /<id>/ws that the bridge connects
    # to).  No bridge on B — it is purely a server.
    seed_web(swift_binary, workdir_b, port_b)
    seed_web_ws(swift_binary, workdir_b)

    # Spawn B (server) first so its web_ws is accepting connections before A's
    # bridge daemon attempts to dial on boot.
    kernel_b = await swift_kernel(workdir_b, port_b)
    kernel_a = await swift_kernel(workdir_a, port_a)

    # Explicit boot call is idempotent; ensures the bridge connection is fully
    # established before we exercise the forward path.
    await kernel_a.call("bridge", "boot")

    # Forward a reflect call through the bridge to B's kernel alias.
    # B dispatches it to its root (core on Swift), which returns the uniform
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

    # Sanity: B's root id is "core" on Swift.
    # Validate only that we got a non-empty string — the alias resolved correctly.
    assert isinstance(reply["id"], str) and reply["id"], (
        f"reflect 'id' should be a non-empty string, got: {reply['id']!r}"
    )

    # Suppress unused-variable warning: kernel_b is held to keep the process
    # alive for the duration of the test (the fixture terminates it on teardown).
    assert kernel_b is not None
