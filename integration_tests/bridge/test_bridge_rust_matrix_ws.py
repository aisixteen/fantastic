"""Bridge integration — Rust in the cross-runtime matrix (WS-only).

Proves that the rust `ws_bridge` agent interoperates correctly across all
runtime pairs. Five forward/reflect test cases cover every directed edge where
rust is involved:

  rust→rust · rust→python · python→rust · rust→swift · swift→rust

Plus one rust↔rust `watch_remote` streaming test.

Setup per forward test: client kernel gets web + web_ws + bridge seeds; server
kernel gets web + web_ws. Server spawns first (the bridge connects eagerly on
boot), then `boot` is an idempotent connect guard.

Root-id note: `_forward_reflect` dispatches via peer_id='kernel' and
target='kernel' — the `kernel` alias resolves to the real root on any runtime
(kernel_state for Python, core for rust/swift), so no forward test hardcodes a
runtime-specific root id.

The stream test uses peer_id='core' literally because watch/emit do a literal
id lookup (no alias), and both sides are rust kernels whose root id is 'core'.
`assert_watch_remote_streams` defaults server_root='core', which is correct here.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws
from helpers.streaming import assert_watch_remote_streams

import os as _os

# Local-loopback (127.0.0.1) bridge addressing — meaningless inside a container.
# The cross-container bridge is covered by test_bridge_container_to_container
# (host.containers.internal); skip this local matrix under the container target.
pytestmark = pytest.mark.skipif(
    _os.environ.get("FANTASTIC_TARGET", "local").strip().lower() == "container",
    reason="local-loopback bridge; container path = test_bridge_container_to_container",
)


async def _forward_reflect(
    client_bin: Path,
    client_spawn: Any,
    server_bin: Path,
    server_spawn: Any,
    parity_tmp: Any,
    free_port: Any,
    tag: str,
) -> None:
    """Shared forward/reflect driver for all cross-runtime pairs.

    Seeds client A (web + web_ws + bridge) and server B (web + web_ws),
    spawns B then A, boots the bridge, and asserts that a `forward` call
    carrying `{type: reflect}` returns the uniform reflect shape
    (id, sentence, tree).

    Both peer_id and target use the 'kernel' alias so the dispatch resolves
    to the correct root id regardless of runtime (kernel_state or core).
    """
    base = parity_tmp(tag)
    wa = base / "A_client"
    wb = base / "B_server"
    wa.mkdir(parents=True)
    wb.mkdir(parents=True)
    pa = free_port()
    pb = free_port()

    seed_web(client_bin, wa, pa)
    seed_web_ws(client_bin, wa)
    # peer_id='kernel' selects the WS path on B using the runtime alias —
    # resolves to 'kernel_state' (python) or 'core' (rust/swift) transparently.
    seed_bridge_ws(client_bin, wa, agent_id="bridge", peer_id="kernel", peer_port=pb)

    seed_web(server_bin, wb, pb)
    seed_web_ws(server_bin, wb)

    await server_spawn(wb, pb)  # server first — bridge dials on boot
    ka = await client_spawn(wa, pa)
    await ka.call("bridge", "boot")

    # target='kernel' resolves to B's root on any runtime.
    reply = await ka.call("bridge", "forward", target="kernel", payload={"type": "reflect"})
    assert isinstance(reply, dict), f"expected dict, got {type(reply)}: {reply}"
    assert "error" not in reply, f"forward returned error: {reply}"
    # Uniform reflect carries id + sentence + tree.
    assert all(k in reply for k in ("id", "sentence", "tree")), (
        f"reply lacks uniform reflect fields: {list(reply.keys())}"
    )


@pytest.mark.asyncio
async def test_rust_rust_ws_forward(rust_binary, rust_kernel, parity_tmp, free_port):
    await _forward_reflect(
        rust_binary,
        rust_kernel,
        rust_binary,
        rust_kernel,
        parity_tmp,
        free_port,
        "rust_rust",
    )


@pytest.mark.asyncio
async def test_rust_python_ws_forward(
    rust_binary, rust_kernel, python_binary, python_kernel, parity_tmp, free_port
):
    await _forward_reflect(
        rust_binary,
        rust_kernel,
        python_binary,
        python_kernel,
        parity_tmp,
        free_port,
        "rust_python",
    )


@pytest.mark.asyncio
async def test_python_rust_ws_forward(
    python_binary, python_kernel, rust_binary, rust_kernel, parity_tmp, free_port
):
    await _forward_reflect(
        python_binary,
        python_kernel,
        rust_binary,
        rust_kernel,
        parity_tmp,
        free_port,
        "python_rust",
    )


@pytest.mark.asyncio
async def test_rust_swift_ws_forward(
    rust_binary, rust_kernel, swift_binary, swift_kernel, parity_tmp, free_port
):
    await _forward_reflect(
        rust_binary,
        rust_kernel,
        swift_binary,
        swift_kernel,
        parity_tmp,
        free_port,
        "rust_swift",
    )


@pytest.mark.asyncio
async def test_swift_rust_ws_forward(
    swift_binary, swift_kernel, rust_binary, rust_kernel, parity_tmp, free_port
):
    await _forward_reflect(
        swift_binary,
        swift_kernel,
        rust_binary,
        rust_kernel,
        parity_tmp,
        free_port,
        "swift_rust",
    )


@pytest.mark.asyncio
async def test_rust_rust_ws_watch_remote_streams_event(
    rust_binary, rust_kernel, parity_tmp, free_port
):
    """Proves that watch_remote delivers a streamed event across the WS bridge
    between two rust kernels.

    peer_id='core' is intentional: watch/emit do a LITERAL id lookup (no alias),
    and both A and B are rust kernels whose root id is 'core'. Using 'kernel'
    here would silently break on a future runtime that remaps the alias.
    assert_watch_remote_streams defaults server_root='core', which matches.
    """
    base = parity_tmp("rust_rust_stream")
    wa = base / "A"
    wb = base / "B"
    wa.mkdir(parents=True)
    wb.mkdir(parents=True)
    pa = free_port()
    pb = free_port()
    for wd, port in [(wa, pa), (wb, pb)]:
        seed_web(rust_binary, wd, port)
        seed_web_ws(rust_binary, wd)
    # peer_id='core' is the LITERAL rust root — required for watch/emit lookup.
    seed_bridge_ws(rust_binary, wa, agent_id="bridge", peer_id="core", peer_port=pb)

    await rust_kernel(wb, pb)
    ka = await rust_kernel(wa, pa)
    await ka.call("bridge", "boot")

    # server_root defaults to 'core' (rust literal root) — matches peer_id above.
    await assert_watch_remote_streams(pa, pb)
