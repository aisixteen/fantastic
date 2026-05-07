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


class _ServerHandle:
    def __init__(self, server: uvicorn.Server, task: asyncio.Task):
        self.server = server
        self.task = task


async def _spawn(agent_id: str, kernel) -> _ServerHandle | None:
    rec = kernel.get(agent_id) or {}
    port_val = rec.get("port")
    if not port_val:
        raise RuntimeError(
            f"webapp {agent_id}: rec.port is required (no default). "
            "Set port on create_agent or via update_agent."
        )
    port = int(port_val)
    app = make_app(agent_id, kernel)
    config = uvicorn.Config(
        app,
        host="0.0.0.0",
        port=port,
        log_level="warning",
        loop="asyncio",
    )
    server = uvicorn.Server(config)

    async def _safe_serve():
        # uvicorn calls sys.exit(1) on bind failure (e.g. port already
        # in use). SystemExit out of an asyncio task aborts the whole
        # event loop — taking the kernel down with one bad webapp.
        # Catch it, log, and return so the kernel keeps running.
        try:
            await server.serve()
        except SystemExit as e:
            logger.error(
                "webapp %s :%d serve exited (code=%s) — port likely in use",
                agent_id,
                port,
                e.code,
            )
        except Exception as e:
            logger.error("webapp %s :%d serve failed: %s", agent_id, port, e)
        finally:
            _servers.pop(agent_id, None)

    task = asyncio.create_task(_safe_serve())
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
    port_val = rec.get("port")
    return {
        "id": id,
        "sentence": "HTTP+WS transport for the kernel.",
        "port": int(port_val) if port_val else None,
        "base_route": rec.get("base_route", ""),
        "running": id in _servers,
        "served_agent_count": len(kernel.list()),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, kernel):
    """Idempotent. Spawns uvicorn on rec.port (required, no default). Returns {running:true}."""
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
