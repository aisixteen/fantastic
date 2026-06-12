"""Bridge streaming integration test — Swift A (client) → Python B (server).

Proves: Swift's bridge can subscribe to Python B's root via `watch_remote`
and receive events forwarded over the WS transport.

Flow:
  1. Seed Python B with web + web_ws; resolve B's literal root id via
     `root_id(python_binary, workdir_b)` — this yields `'kernel_state'`, NOT
     `'core'` (the root-id asymmetry: rust/swift use `core`, python uses
     `kernel_state`). Hardcoding `'core'` here would be WRONG.
  2. Seed Swift A with web + web_ws + a `kernel_bridge` agent whose
     `peer_id` is set to `server_root` (`'kernel_state'`). The bridge dials
     `ws://127.0.0.1:<port_b>/<server_root>/ws` and auto-watches that inbox.
  3. Spawn B first (peer must be reachable before A's bridge dials in),
     then A.
  4. Boot the bridge on A, then run `assert_watch_remote_streams` which:
       a. Calls `bridge.watch_remote(target=server_root)` on A.
       b. Emits a nonced probe on B's root.
       c. Asserts the probe event propagates back to A's bridge inbox.

Both binaries are required; the test skips cleanly when either is absent.
"""

from __future__ import annotations

import pytest

from helpers.seeding import root_id, seed_bridge_ws, seed_web, seed_web_ws
from helpers.streaming import assert_watch_remote_streams


@pytest.mark.asyncio
async def test_swift_python_ws_watch_remote_streams_event(
    python_binary,
    swift_binary,
    python_kernel,
    swift_kernel,
    parity_tmp,
    free_port,
) -> None:
    """Swift A subscribes to Python B's root via watch_remote and receives a
    nonced probe event — proving cross-runtime WS bridge streaming works with
    Python's `kernel_state` root id (not `core`).
    """
    base = parity_tmp("sw_py_ws_stream")
    workdir_a = base / "A_swift"
    workdir_b = base / "B_python"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # --- Seed Python B (server / watched kernel) ---
    seed_web(python_binary, workdir_b, port_b)
    seed_web_ws(python_binary, workdir_b)

    # Resolve B's LITERAL root id — `'kernel_state'` for python, `'core'` for
    # rust/swift. `watch_remote` + `ws_emit` both do a literal id lookup (the
    # `kernel` alias is only valid for dispatch, not for watch/emit targets).
    server_root: str = root_id(python_binary, workdir_b)

    # --- Seed Swift A (client / bridge holder) ---
    seed_web(swift_binary, workdir_a, port_a)
    # WS opt-in: required so A's native WS server is live (orchestrator + watch).
    seed_web_ws(swift_binary, workdir_a)
    # Bridge dials `ws://127.0.0.1:<port_b>/<server_root>/ws` on boot.
    seed_bridge_ws(
        swift_binary,
        workdir_a,
        agent_id="bridge",
        peer_id=server_root,  # 'kernel_state' — not 'core'
        peer_port=port_b,
    )

    # Spawn B first so the peer WS surface is reachable before A's bridge dials in.
    kernel_b = await python_kernel(workdir_b, port_b)  # noqa: F841 — kept alive by fixture
    kernel_a = await swift_kernel(workdir_a, port_a)  # noqa: F841
    # Boot the bridge: opens the WS connection to B and starts the read loop.
    await kernel_a.call("bridge", "boot")

    # Drive the full watch_remote round-trip and assert the probe event arrives.
    await assert_watch_remote_streams(port_a, port_b, server_root=server_root)
