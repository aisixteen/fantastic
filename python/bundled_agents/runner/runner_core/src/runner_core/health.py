"""Shared WS liveness probe for the runner bundles.

Both local_runner and ssh_runner prove a spawned kernel is alive AND
answering by opening the kernel's WS verb channel
(`ws://localhost:<port>/fs_loader/ws`), sending a `reflect` call frame,
and expecting a `reply` within ~2s. WS is the verb channel — a reply
proves the kernel is up and dispatching, not merely that something is
bound to the port.

local_runner probes the discovered web port directly; ssh_runner probes
the local end of its SSH tunnel — same frame, same wire.
"""

from __future__ import annotations

import asyncio
import json

import websockets


async def _ws_health(port: int) -> bool:
    """Connect to `ws://localhost:<port>/fs_loader/ws`, send a reflect
    frame, expect a reply within 2s. WS is the verb channel — this proves
    the kernel is alive AND answering, not just that something is bound to
    the port."""
    url = f"ws://localhost:{port}/fs_loader/ws"
    try:
        async with asyncio.timeout(2):
            async with websockets.connect(url) as ws:
                await ws.send(
                    json.dumps(
                        {
                            "type": "call",
                            "target": "fs_loader",
                            "payload": {"type": "reflect"},
                            "id": "h",
                        }
                    )
                )
                while True:
                    msg = json.loads(await ws.recv())
                    if msg.get("id") == "h" and msg.get("type") in (
                        "reply",
                        "error",
                    ):
                        return msg.get("type") == "reply"
    except (TimeoutError, OSError, websockets.WebSocketException):
        return False
