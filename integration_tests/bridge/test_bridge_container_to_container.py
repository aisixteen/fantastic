"""Container → container bridge — the "each container is a unit at host:port"
model, with NO shared / user-defined network.

Two kernel CONTAINERS (A = client, B = server), each PUBLISHED on the host
(`-p port:port`, all interfaces). A's `ws_bridge` dials B through the
built-in host-gateway name `host.containers.internal:<port_b>` — the host
forwards that to B's published port. This proves container↔container bridging
works over plain published ports, exactly the way you'd reach a container on
ANOTHER machine by its `ip:port` (the bridge is weak-binding by URL — it doesn't
care whether the peer is a local container, a remote container, or a bare host).

Runs ONLY under `FANTASTIC_TARGET=container` (skips otherwise — locally the
`test_bridge_python_python_ws` forward path covers the same bridge surface).
"""

from __future__ import annotations

import os
import socket
import subprocess

import pytest

from helpers.kernel_proc import KernelProc
from helpers.launcher import CONTAINER_PEER_HOST, ContainerLauncher, resolve_engine
from helpers.seeding import seed_bridge_ws, seed_web, seed_web_ws
from helpers.ws import ws_call

_TARGET = os.environ.get("FANTASTIC_TARGET", "local").strip().lower()
_IMAGE = os.environ.get("FANTASTIC_IMAGE", "fantastic:latest")


def _free_port() -> int:
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]
    finally:
        s.close()


def _launcher_or_skip() -> ContainerLauncher:
    if _TARGET != "container":
        pytest.skip("container→container bridge runs only under FANTASTIC_TARGET=container")
    engine = resolve_engine()
    if engine is None:
        pytest.skip("no podman/docker found")
    if subprocess.run([engine, "image", "inspect", _IMAGE], capture_output=True).returncode != 0:
        pytest.skip(f"image {_IMAGE!r} not built (run `sh container/build.sh`)")
    return ContainerLauncher(_IMAGE, "python", engine)


@pytest.mark.asyncio
async def test_container_to_container_bridge_forward(parity_tmp) -> None:
    """A container's bridge forwards a call to ANOTHER container via the host
    gateway — published ports only, no shared network."""
    launcher = _launcher_or_skip()
    base = parity_tmp("ct2ct")
    wa = base / "A"
    wb = base / "B"
    wa.mkdir(parents=True)
    wb.mkdir(parents=True)
    port_a = _free_port()
    port_b = _free_port()

    # Seed both sides (web + web_ws) — one-shots run inside the image.
    for wd, port in [(wa, port_a), (wb, port_b)]:
        seed_web(launcher, wd, port)
        seed_web_ws(launcher, wd)

    # Only A gets a bridge; it dials B at the HOST-GATEWAY name (not 127.0.0.1,
    # which inside A is A's own loopback). peer_id="kernel" → ws://B/kernel/ws.
    seed_bridge_ws(
        launcher,
        wa,
        agent_id="bridge",
        peer_id="kernel",
        peer_port=port_b,
        host=CONTAINER_PEER_HOST,
    )

    procs: list[KernelProc] = []
    try:
        # B (server) first — both PUBLISHED on all interfaces so the host gateway
        # can forward A→B. The host still reaches each at 127.0.0.1:<port>.
        kb = launcher.start_daemon(wb, port_b, label="B", publish_all=True)
        procs.append(kb)
        await kb.wait_ready()
        ka = launcher.start_daemon(wa, port_a, label="A", publish_all=True)
        procs.append(ka)
        await ka.wait_ready()

        # Idempotent connect guard, then forward a list_agents to B over the bridge.
        await ws_call(port_a, "bridge", "boot")
        reply = await ws_call(
            port_a, "bridge", "forward", target="kernel", payload={"type": "list_agents"}
        )

        assert isinstance(reply, dict), f"expected dict, got {type(reply)}: {reply}"
        assert "agents" in reply, f"list_agents reply missing 'agents': {reply}"
        ids = {a.get("id") for a in reply["agents"] if isinstance(a, dict)}
        # B's seeded `web` coming back proves the reply crossed container→container.
        assert "web" in ids, f"B's agents should include 'web' (cross-container reply): {ids}"
    finally:
        for p in procs:
            p.terminate()
