"""Minimal WebSocket client for the fantastic call/reply protocol.

Wire shape (matches python/bundled_agents/web/host/_proxy.py — the
canonical reference):

    C → S  {"type": "call",  "target": "<agent_id>", "payload": {...}, "id": "<corr_id>"}
    S → C  {"type": "reply", "id": "<corr_id>", "data": {...}}
    S → C  {"type": "event", "payload": {...}}        # watcher fanout

This module wraps the `websockets` client lib in a thin helper that
exposes one async function — `ws_call(port, target, verb, **args)` —
returning the unwrapped reply data.
"""

from __future__ import annotations

import asyncio
import json
import uuid
from typing import Any

import websockets


async def ws_emit(
    port: int,
    target: str,
    *,
    host: str = "127.0.0.1",
    timeout: float = 10.0,
    settle: float = 0.2,
    **payload: Any,
) -> None:
    """Open a one-shot WS and send an `{type:"emit", target, payload}`
    frame — fire-and-forget into `target`'s inbox (no reply). Mirrors
    the canonical `_on_emit` handler. Keeps the socket open briefly
    (`settle`) so the server processes the frame before close.

    Used by streaming tests to trigger an emit on a remote kernel
    that a `watch_remote` subscription should observe.
    """
    frame = {"type": "emit", "target": target, "payload": dict(payload)}
    # Connect on the target's path so the route resolves; the `target`
    # in the frame is what _on_emit dispatches on.
    url = f"ws://{host}:{port}/{target}/ws"
    async with websockets.connect(url, open_timeout=timeout) as ws:
        await ws.send(json.dumps(frame))
        await asyncio.sleep(settle)


async def ws_call(
    port: int,
    agent_id: str,
    verb: str,
    *,
    host: str = "127.0.0.1",
    timeout: float = 10.0,
    **args: Any,
) -> dict[str, Any]:
    """Open a one-shot WS to `ws://{host}:{port}/{agent_id}/ws`, send a
    `call` frame with `{type:verb, ...args}` as payload, await the
    matching `reply` frame, return `reply.data`.

    Note `agent_id` (not `target`) so callers can pass `target=...`
    as a verb arg (e.g. `bridge.forward(target=..., payload=...)`)
    without colliding with this helper's first parameter.

    Drops the WS after the round-trip. Not appropriate for streaming
    consumers — use `ws_session` (below) when you need to drain
    `event` frames after the initial reply.
    """
    corr_id = f"call_{uuid.uuid4().hex[:8]}"
    payload = {"type": verb, **args}
    frame = {
        "type": "call",
        "target": agent_id,
        "payload": payload,
        "id": corr_id,
    }
    url = f"ws://{host}:{port}/{agent_id}/ws"

    async with websockets.connect(url, open_timeout=timeout) as ws:
        await ws.send(json.dumps(frame))
        # Drain frames until we see the matching reply. Server may
        # emit unrelated `event` frames before the reply lands; skip
        # those.
        while True:
            raw = await ws.recv()
            msg = json.loads(raw)
            if msg.get("type") == "reply" and msg.get("id") == corr_id:
                return msg.get("data", {})


class ws_session:
    """Async context manager that holds an open WS to the given
    target. Use when a test needs to drain `event` frames after
    sending a verb (e.g. streaming tokens from an LLM backend or
    watching an inbox).

        async with ws_session(port, "fm") as ws:
            await ws.send_call("send", text="hello")
            async for evt in ws.events():
                if evt["type"] == "done":
                    break
                ...
    """

    def __init__(self, port: int, target: str, *, host: str = "127.0.0.1", timeout: float = 10.0):
        self._target = target
        self._url = f"ws://{host}:{port}/{target}/ws"
        self._timeout = timeout
        self._ws: Any | None = None
        self._next_corr = 0

    async def __aenter__(self):
        self._ws = await websockets.connect(self._url, open_timeout=self._timeout)
        return self

    async def __aexit__(self, exc_type, exc, tb):
        if self._ws is not None:
            await self._ws.close()
            self._ws = None

    def _mint_corr(self) -> str:
        self._next_corr += 1
        return f"call_{self._next_corr}"

    async def send_call(self, verb: str, **args: Any) -> str:
        """Send a `call` frame to the session's target; return the
        correlation id for matching. The `target` is set explicitly —
        Python's `_on_call` requires it (only Swift's server defaults
        to the URL-path agent)."""
        assert self._ws is not None
        corr_id = self._mint_corr()
        frame = {
            "type": "call",
            "target": self._target,
            "payload": {"type": verb, **args},
            "id": corr_id,
        }
        await self._ws.send(json.dumps(frame))
        return corr_id

    async def await_reply(self, corr_id: str) -> dict[str, Any]:
        """Drain frames until the matching `reply` arrives; return its data."""
        assert self._ws is not None
        while True:
            raw = await self._ws.recv()
            msg = json.loads(raw)
            if msg.get("type") == "reply" and msg.get("id") == corr_id:
                return msg.get("data", {})

    async def events(self):
        """Async generator yielding every non-reply frame (typically
        `event` frames carrying inbox payloads). Caller breaks out
        when a sentinel event is observed.
        """
        assert self._ws is not None
        while True:
            raw = await self._ws.recv()
            msg = json.loads(raw)
            if msg.get("type") != "reply":
                yield msg
