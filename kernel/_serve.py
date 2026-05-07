"""Headless `serve`: boot kernel, ensure a webapp agent, idle forever."""

from __future__ import annotations

import asyncio
import sys

from kernel._bundles import _find_bundle_module, _seed_singletons
from kernel._kernel import Kernel
from kernel._lock import acquire_serve_lock


async def cmd_serve(port: int | None = None) -> None:
    """Headless: boot kernel, ensure a webapp agent on `port`, idle forever.

    `port=None` → pick an ephemeral free port. No hardcoded default;
    operators see exactly which port was chosen instead of guessing.
    """
    if port is None:
        import socket as _socket

        s = _socket.socket()
        s.bind(("", 0))
        port = s.getsockname()[1]
        s.close()
        print(f"[serve] no --port given, picked free port {port}", file=sys.stderr)
    acquire_serve_lock(port)
    k = Kernel()
    await _seed_singletons(k)
    webapp_mod = _find_bundle_module("webapp")
    if webapp_mod is None:
        raise RuntimeError("webapp bundle not installed; run `uv sync`")
    web_id: str | None = None
    for a in k.list():
        if a.get("handler_module") == webapp_mod and int(a.get("port", 0)) == port:
            web_id = a["id"]
            break
    if web_id is None:
        rec = await k.send(
            "core",
            {
                "type": "create_agent",
                "handler_module": webapp_mod,
                "port": port,
            },
        )
        web_id = rec["id"] if isinstance(rec, dict) else None
    if web_id:
        await k.send(web_id, {"type": "boot"})
    print(f"[serve] kernel up; web={web_id} port={port}", flush=True)
    try:
        while True:
            await asyncio.sleep(3600)
    except (KeyboardInterrupt, asyncio.CancelledError):
        pass
