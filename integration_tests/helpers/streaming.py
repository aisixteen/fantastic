"""Shared driver for `watch_remote` streaming integration tests.

The flow is identical regardless of which runtimes are paired — only
the seeding/spawn differs. Each test seeds A (client, holds the
bridge) + B (server), spawns B-then-A, boots the bridge, then calls
`assert_watch_remote_streams(port_a, port_b)`.

Mechanism:
  1. Open a watcher session on A's bridge inbox.
  2. A.bridge.watch_remote(target="core") → sends {watch, src:core}
     to B; B registers a watch on its core.
  3. Emit a uniquely-nonced payload on B's core.
  4. B fans it out to the watcher → {event} frame over the bridge WS
     → A.bridge re-emits on its own inbox → our session sees it.
"""

from __future__ import annotations

import asyncio
import uuid

from .ws import ws_emit, ws_session


async def assert_watch_remote_streams(
    port_a: int,
    port_b: int,
    *,
    bridge_id: str = "bridge",
    server_root: str = "core",
    timeout: float = 8.0,
) -> None:
    """Drive + assert a full watch_remote round-trip. Raises
    AssertionError (or asyncio.TimeoutError) on failure.

    `server_root` is B's LITERAL root agent id — `watch`/`emit` do a
    literal id lookup (no `kernel` alias), so this must be the real
    root (`fs_loader` for python, `core` for rust/swift)."""
    nonce = uuid.uuid4().hex[:12]

    async with ws_session(port_a, bridge_id) as sess:
        # Subscribe to B's root via the bridge.
        corr = await sess.send_call("watch_remote", target=server_root)
        reply = await sess.await_reply(corr)
        assert reply.get("ok") is True, f"watch_remote failed: {reply}"
        assert reply.get("watching") == server_root

        # Let B register the watch before emitting.
        await asyncio.sleep(0.4)

        # Trigger an emit on B's root carrying our nonce.
        await ws_emit(port_b, server_root, type="stream_probe", nonce=nonce)

        async def _await_probe():
            async for evt in sess.events():
                payload = evt.get("payload") or {}
                if payload.get("type") == "stream_probe" and payload.get("nonce") == nonce:
                    return payload
            return None

        probe = await asyncio.wait_for(_await_probe(), timeout=timeout)

    assert probe is not None, "watch_remote did not deliver the probe event"
    assert probe.get("nonce") == nonce
