"""Bridge integration test — Python ↔ Python over WS, forward (request/reply).

Proves the WS-only kernel_bridge FORWARD path: A sends a call frame over WS
and gets a synchronous reply. This is the request/reply matrix; see the stream
sibling for the watch/emit surface.

Baseline harness validator for the WS-only kernel_bridge. A's bridge opens a
WS to B's `web_ws` surface and ships raw call frames; B's `web_ws` dispatches
`kernel.send(target, payload)` exactly like a browser frame.
**No B-side bridge agent needed.**

Topology:

    Workdir A (client)           Workdir B (server)
    ─────────────────────        ──────────────────────────────
    fs_loader (root)             fs_loader (root)
    web (port_a)                 web (port_b)
    web_ws                       web_ws           ← serves /<id>/ws
    bridge (transport=ws) ──ws──▶ ws://127.0.0.1:port_b/kernel/ws
        forward(target="kernel", payload)
          → {type:call, target:"kernel", payload}
                                       ↓
                              B.web_ws._on_call → kernel.send("kernel", payload)
                                       ↓ kernel alias resolves to B's root (fs_loader)
                              reply rides back over the same socket

The bridge dials the `kernel` ALIAS path (`ws://B/kernel/ws`) — not a
hardcoded `core` path. On both kernels `kernel` is a universal alias that
resolves to the runtime's actual root: `fs_loader` on Python, `core` on
Swift/Rust. Forwarded call frames also target `"kernel"`, which B resolves
locally. This means the test is runtime-agnostic and never hard-codes a
root id.

Ordering: spawn B first (the bridge connects eagerly at boot, so the server
must be up). After both are ready we call `boot` once — it returns
`{already:true}` if the daemon already connected, or connects now if its
startup boot raced B.
"""

from __future__ import annotations

from pathlib import Path
from typing import Callable

import pytest

from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws
from helpers.kernel_proc import KernelProc


async def _bring_up(
    python_binary: Path,
    python_kernel: Callable[..., KernelProc],
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
    tag: str,
) -> tuple[KernelProc, KernelProc]:
    base = parity_tmp(tag)
    workdir_a = base / "A"
    workdir_b = base / "B"
    workdir_a.mkdir(parents=True)
    workdir_b.mkdir(parents=True)

    port_a = free_port()
    port_b = free_port()

    # Both sides: web + web_ws (so each is drivable + serves /<id>/ws).
    for wd, port in [(workdir_a, port_a), (workdir_b, port_b)]:
        seed_web(python_binary, wd, port)
        seed_web_ws(python_binary, wd)

    # Only A gets a bridge. peer_id="kernel" → dial ws://B/kernel/ws.
    # On Python, the "kernel" alias resolves to B's root ("fs_loader").
    seed_bridge_ws(
        python_binary,
        workdir_a,
        agent_id="bridge",
        peer_id="kernel",
        peer_port=port_b,
    )

    # Spawn B (server) first, then A (client). A's daemon boots the
    # bridge → connects to the already-up B.
    kernel_b = await python_kernel(workdir_b, port_b)
    kernel_a = await python_kernel(workdir_a, port_a)

    # Idempotent connect guard — no-op if the bridge already connected
    # during daemon boot; connects now if startup boot raced B's readiness.
    await kernel_a.call("bridge", "boot")
    return kernel_a, kernel_b


@pytest.mark.asyncio
async def test_python_python_ws_forward_reflect(
    python_binary: Path,
    python_kernel: Callable[..., KernelProc],
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
) -> None:
    """Forward a reflect call across the bridge; reply carries B's id + tree."""
    kernel_a, _ = await _bring_up(
        python_binary, python_kernel, parity_tmp, free_port, "py_py_ws_reflect"
    )

    reply = await kernel_a.call(
        "bridge",
        "forward",
        target="kernel",
        payload={"type": "reflect"},
    )

    assert isinstance(reply, dict), f"expected dict, got {type(reply)}: {reply}"
    assert "error" not in reply, f"forward returned error: {reply}"
    # Uniform reflect carries id + tree (default tree=all).
    assert "id" in reply and "tree" in reply, (
        f"reply lacks uniform reflect fields: {list(reply.keys())}"
    )


@pytest.mark.asyncio
async def test_python_python_ws_forward_list_agents(
    python_binary: Path,
    python_kernel: Callable[..., KernelProc],
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
) -> None:
    """Forward a list_agents call across the bridge; reply proves B's tree arrived."""
    kernel_a, _ = await _bring_up(
        python_binary, python_kernel, parity_tmp, free_port, "py_py_ws_list"
    )

    reply = await kernel_a.call(
        "bridge",
        "forward",
        target="kernel",
        payload={"type": "list_agents"},
    )

    assert isinstance(reply, dict), f"expected dict, got {reply}"
    assert "agents" in reply, f"list_agents reply missing 'agents': {reply}"
    ids = {a.get("id") for a in reply["agents"] if isinstance(a, dict)}
    # B was seeded with a web agent (id="web") — its presence proves the
    # remote agent tree came back across the bridge.
    assert "web" in ids, f"B's list_agents should include 'web', got {ids}"
