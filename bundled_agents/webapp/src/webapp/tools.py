"""webapp bundle — uvicorn HTTP+WS transport as an agent.

Verbs:
  reflect   -> {sentence, port, base_route, served_agent_count, running}
  boot      -> spawn uvicorn task if not running (idempotent)
  stop      -> cancel uvicorn task
"""

from __future__ import annotations

import asyncio
import logging

import uvicorn

from .app import make_app

logger = logging.getLogger(__name__)

_servers: dict[str, "_ServerHandle"] = {}

DEFAULT_PORT = 8888


class _ServerHandle:
    def __init__(self, server: uvicorn.Server, task: asyncio.Task):
        self.server = server
        self.task = task


async def _spawn(agent_id: str, kernel) -> _ServerHandle | None:
    rec = kernel.get(agent_id) or {}
    port = int(rec.get("port", DEFAULT_PORT))
    app = make_app(agent_id, kernel)
    config = uvicorn.Config(
        app,
        host="0.0.0.0",
        port=port,
        log_level="warning",
        loop="asyncio",
    )
    server = uvicorn.Server(config)
    task = asyncio.create_task(server.serve())
    handle = _ServerHandle(server, task)
    _servers[agent_id] = handle
    print(f"  [web] {agent_id} listening on http://localhost:{port}/")
    return handle


async def _shutdown(agent_id: str) -> bool:
    handle = _servers.pop(agent_id, None)
    if not handle:
        return False
    handle.server.should_exit = True
    try:
        await asyncio.wait_for(handle.task, timeout=3.0)
    except (asyncio.TimeoutError, asyncio.CancelledError):
        handle.task.cancel()
    return True


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + uvicorn port + running flag (process-local; read via the live serve for truth)."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "HTTP+WS transport for the kernel.",
        "port": int(rec.get("port", DEFAULT_PORT)),
        "base_route": rec.get("base_route", ""),
        "running": id in _servers,
        "served_agent_count": len(kernel.list()),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, kernel):
    """Idempotent. Spawns uvicorn on rec.port (default 8888) if not already running. Returns {running:true}."""
    if id not in _servers:
        await _spawn(id, kernel)
    return {"running": True}


async def _stop(id, payload, kernel):
    """No args. Asks uvicorn to shut down; cancels its task. Returns {stopped:bool}."""
    ok = await _shutdown(id)
    return {"stopped": ok}


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "stop": _stop,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"web: unknown type {t!r}"}
    return await fn(id, payload, kernel)
