"""webapp bundle — uvicorn HTTP host.

`web` owns the FastAPI app + uvicorn task. Rendering routes are baked
in (`/`, `/<id>/`, `/<id>/file/<path>`, transport.js, favicon). Verb-
invocation routes are NOT — those live in sub-agent bundles (`web_ws`,
`web_rest`) that declare their routes via the duck-typed `get_routes`
verb. On boot, web walks its children and mounts whatever they return
onto the live FastAPI app via `app.add_api_route` /
`app.add_api_websocket_route`. Children emit `routes_changed` on
themselves to hot-swap their surface.

Verbs:
  reflect   -> {sentence, port, served_agent_count, running, surfaces}
  boot      -> spawn uvicorn; mount each child's surface
  stop      -> cancel uvicorn task
  mount     -> (re)mount a child's routes (idempotent)
  unmount   -> remove a child's routes from the live app
"""

from __future__ import annotations

import asyncio
import logging
import sys
from typing import Any

import uvicorn
from fastapi import FastAPI

from .app import make_app

logger = logging.getLogger(__name__)

_servers: dict[str, "_ServerHandle"] = {}


class _ServerHandle:
    def __init__(self, server: uvicorn.Server, task: asyncio.Task, app: FastAPI):
        self.server = server
        self.task = task
        self.app = app
        # child_id -> list of Route objects mounted on `app.routes` for that child
        self.routes_by_child: dict[str, list[Any]] = {}


async def _mount_surface(handle: _ServerHandle, kernel, child_id: str) -> None:
    """Pull `get_routes` from one child and mount the returned routes
    onto the live FastAPI app. No-op if the child doesn't answer."""
    try:
        r = await kernel.send(child_id, {"type": "get_routes"})
    except Exception as e:
        logger.warning("web: get_routes failed for %s: %s", child_id, e)
        return
    if not isinstance(r, dict):
        return
    routes = r.get("routes") or []
    mounted: list[Any] = []
    for spec in routes:
        kind = spec.get("kind")
        path = spec.get("path")
        endpoint = spec.get("endpoint")
        if not path or not callable(endpoint):
            continue
        if kind == "websocket":
            handle.app.add_api_websocket_route(path, endpoint)
        else:
            method = (spec.get("method") or "GET").upper()
            handle.app.add_api_route(path, endpoint, methods=[method])
        # The route just appended is the last entry in app.routes.
        mounted.append(handle.app.routes[-1])
    if mounted:
        handle.routes_by_child[child_id] = mounted
        logger.info("web: mounted %d route(s) for %s", len(mounted), child_id)


def _unmount_surface(handle: _ServerHandle, child_id: str) -> None:
    """Remove every route that belongs to a child from the running app.
    Starlette walks `app.routes` per-request so removal takes effect
    immediately."""
    routes = handle.routes_by_child.pop(child_id, [])
    if not routes:
        return
    keep = [r for r in handle.app.routes if r not in routes]
    handle.app.router.routes[:] = keep
    logger.info("web: unmounted %d route(s) for %s", len(routes), child_id)


async def _mount_all_surfaces(handle: _ServerHandle, kernel, web_agent_id: str) -> None:
    """Walk web's children and mount each one's surface."""
    web_agent = kernel.ctx.agents.get(web_agent_id)
    if web_agent is None:
        return
    for cid in list(web_agent._children.keys()):
        await _mount_surface(handle, kernel, cid)


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
    handle = _ServerHandle(server, task, app)
    _servers[agent_id] = handle
    print(
        f"  [web] {agent_id} listening on http://localhost:{port}/",
        file=sys.stderr,
    )
    return handle


async def _stop_uvicorn(agent_id: str) -> bool:
    handle = _servers.pop(agent_id, None)
    if not handle:
        return False
    handle.server.should_exit = True
    try:
        await asyncio.wait_for(handle.task, timeout=3.0)
    except (asyncio.TimeoutError, asyncio.CancelledError):
        handle.task.cancel()
    return True


async def on_delete(agent):
    """Cascade hook — drains uvicorn so the HTTP server doesn't
    outlive the agent record."""
    await _stop_uvicorn(agent.id)


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + uvicorn port + running flag + currently mounted surfaces (process-local; reflect the live serve for truth)."""
    rec = kernel.get(id) or {}
    port_val = rec.get("port")
    handle = _servers.get(id)
    surfaces: dict[str, list[str]] = {}
    if handle:
        for cid, routes in handle.routes_by_child.items():
            surfaces[cid] = [getattr(r, "path", str(r)) for r in routes]
    return {
        "id": id,
        "sentence": "HTTP host — rendering routes baked in; call surfaces mounted from sub-agents.",
        "port": int(port_val) if port_val else None,
        "running": id in _servers,
        "served_agent_count": len(kernel.list()),
        "surfaces": surfaces,
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, kernel):
    """Idempotent. Spawns uvicorn on rec.port (required, no default), then walks this web's children and mounts each one's call-surface routes via duck-typed `get_routes`. Returns {running:true, surfaces:[...]}."""
    if id not in _servers:
        await _spawn(id, kernel)
    handle = _servers.get(id)
    if handle is None:
        return {"running": False}
    await _mount_all_surfaces(handle, kernel, id)
    return {"running": True, "surfaces": list(handle.routes_by_child.keys())}


async def _stop(id, payload, kernel):
    """No args. Asks uvicorn to shut down; cancels its task. Returns {stopped:bool}."""
    ok = await _stop_uvicorn(id)
    return {"stopped": ok}


async def _mount(id, payload, kernel):
    """args: child_id:str (req). Pulls `get_routes` from the named child and (re)mounts its routes onto the live app. Idempotent — unmounts any prior routes for the child first."""
    child_id = payload.get("child_id")
    if not child_id:
        return {"error": "web.mount: child_id required"}
    handle = _servers.get(id)
    if handle is None:
        return {"error": "web.mount: not running"}
    _unmount_surface(handle, child_id)
    await _mount_surface(handle, kernel, child_id)
    return {
        "mounted": [
            getattr(r, "path", str(r)) for r in handle.routes_by_child.get(child_id, [])
        ]
    }


async def _unmount(id, payload, kernel):
    """args: child_id:str (req). Removes every route belonging to the named child from the live app."""
    child_id = payload.get("child_id")
    if not child_id:
        return {"error": "web.unmount: child_id required"}
    handle = _servers.get(id)
    if handle is None:
        return {"error": "web.unmount: not running"}
    _unmount_surface(handle, child_id)
    return {"unmounted": child_id}


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "stop": _stop,
    "mount": _mount,
    "unmount": _unmount,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"web: unknown type {t!r}"}
    return await fn(id, payload, kernel)
