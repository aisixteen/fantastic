"""webapp bundle — uvicorn HTTP host.

`web` owns the FastAPI app + uvicorn task. Rendering routes are baked
in (`/`, `/<id>/file/<path>`, favicon — no server-side `/<id>/` render
route and no transport.js; all UI is the TS frontend). Verb-
invocation routes are NOT — those live in sub-agent bundles (`web_ws`,
`web_rest`) that DECLARE their routes via the duck-typed `get_routes`
verb (pure data: `{kind, method?, path}`) and hand over the matching LIVE
endpoint callables through the duck-typed module hook
`route_endpoints(agent) -> {path: endpoint}`. On boot, web walks its children,
pulls both halves and mounts them onto the live FastAPI app via
`app.add_api_route` / `app.add_api_websocket_route`. Children emit
`routes_changed` on themselves to hot-swap their surface.

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
from typing import Any

import uvicorn
from fastapi import FastAPI

from kernel._kernel import _current_sender, _current_task

from .app import DEFAULT_HOST, extra_origins, make_app, own_origins

logger = logging.getLogger(__name__)

_servers: dict[str, "_ServerHandle"] = {}


class _ServerHandle:
    def __init__(
        self,
        server: uvicorn.Server,
        task: asyncio.Task | None,
        app: FastAPI,
        port: int = 0,
    ):
        self.server = server
        self.task = task  # None until _start_serving() schedules the serve loop
        self.app = app
        self.port = port
        # child_id -> list of Route objects mounted on `app.routes` for that child
        self.routes_by_child: dict[str, list[Any]] = {}


async def _mount_surface(handle: _ServerHandle, kernel, child_id: str) -> None:
    """Mount one child's call surface. Two halves, one path each:

    - the DECLARATION — the `get_routes` reply, pure JSON data, so it renders
      over REST and persists into the ledger like any other reply;
    - the LIVE endpoints — the child bundle's `route_endpoints(agent)` module
      hook, discovered like `on_delete`, keyed by the declared path.

    A child that answers no routes has no surface (no-op). A child that DECLARES
    routes but exposes no hook — or no endpoint for a declared path — is a broken
    leg: fail loud at boot, never mount half a surface."""
    try:
        r = (
            await kernel.send([{"target": child_id, "payload": {"type": "get_routes"}}])
        )[0]["result"]
    except Exception as e:
        logger.warning("web: get_routes failed for %s: %s", child_id, e)
        return
    if not isinstance(r, dict):
        return
    routes = r.get("routes") or []
    if not routes:
        return
    child = kernel.ctx.agents.get(child_id)
    hook = child.module_hook("route_endpoints") if child is not None else None
    if hook is None:
        raise RuntimeError(
            f"web: {child_id} declares {len(routes)} route(s) but its bundle "
            "exposes no route_endpoints(agent) hook"
        )
    endpoints = hook(child)
    resolved: list[tuple[dict, str, Any]] = []
    for spec in routes:
        path = spec.get("path")
        endpoint = endpoints.get(path)
        if not path or not callable(endpoint):
            raise RuntimeError(
                f"web: {child_id} declares route {path!r} but route_endpoints() "
                "hands over no endpoint for it"
            )
        resolved.append((spec, path, endpoint))
    # REPLACE, never append: a re-boot re-mounts every surface, and a second copy
    # of a child's routes outlived its `unmount` (which removes the routes it
    # recorded — the newest list only), so the first copy kept serving.
    _unmount_surface(handle, child_id)
    mounted: list[Any] = []
    for spec, path, endpoint in resolved:
        if spec.get("kind") == "websocket":
            handle.app.add_api_websocket_route(path, endpoint)
        else:
            method = (spec.get("method") or "GET").upper()
            handle.app.add_api_route(path, endpoint, methods=[method])
        # The route just appended is the last entry in app.routes.
        mounted.append(handle.app.routes[-1])
    handle.routes_by_child[child_id] = mounted
    logger.info("web: mounted %d route(s) for %s", len(mounted), child_id)
    _order_routes(handle)


def _order_routes(handle: _ServerHandle) -> None:
    """A LITERAL PATH BEATS A PATTERN. Routing is first-match-wins in list order
    (starlette), and legs append their routes in MOUNT order — which on a hydrated
    host is the children's folder order. A `web_ws` leg declares the catch-all
    `/{host_id}/ws`; mounted before a leg that declares the literal
    `/web_ui/ws`, it swallowed it (measured: a CLI canvas leg sorted first and the
    UI socket went dead on every rehydrate, while a first run's creation order
    happened to be right). So the live table is ordered by how many PARAMETERS a
    path has — fewer first; equal counts keep their mount order (a stable sort)."""
    handle.app.router.routes.sort(key=lambda r: str(getattr(r, "path", "")).count("{"))


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
    for cid in web_agent.child_ids():
        await _mount_surface(handle, kernel, cid)


async def _spawn(agent_id: str, kernel) -> _ServerHandle | None:
    """Build the FastAPI app + uvicorn server and register the handle —
    but do NOT start serving yet. The caller mounts child surfaces onto the
    app first, then calls `_start_serving`, so the port never accepts a
    request before its full route table is in place (no mount race; parity
    with the rust host, which builds its whole router before binding)."""
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
        # LOOPBACK unless the record names a host (`host: "0.0.0.0"` to publish).
        host=rec.get("host") or DEFAULT_HOST,
        port=port,
        log_level="warning",
        loop="asyncio",
    )
    server = uvicorn.Server(config)
    handle = _ServerHandle(server, None, app, port=port)
    _servers[agent_id] = handle
    return handle


def _start_serving(agent_id: str, handle: _ServerHandle) -> None:
    """Schedule the uvicorn serve loop for an already-built (and already
    route-mounted) handle. Idempotent: a no-op if it's already serving."""
    if handle.task is not None:
        return
    server = handle.server
    port = handle.port

    async def _safe_serve():
        # NOT a child of the boot dispatch: this task outlives it, and every
        # request task uvicorn creates inherits the context here. Left as it
        # was, each REST-ingress task named a parent pid that had ended
        # (measured 2026-09-16: `parent: 7`, no such row).
        _current_task.set(None)
        # The request tasks run AS THIS HOST, stated here rather than inherited
        # from whoever dispatched `boot` (audit 2026-09-28, d9). Every route that
        # sends also wraps its send in `sender_context`.
        _current_sender.set(agent_id)
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

    handle.task = asyncio.create_task(_safe_serve())


async def _stop_uvicorn(agent_id: str) -> bool:
    handle = _servers.pop(agent_id, None)
    if not handle:
        return False
    handle.server.should_exit = True
    if handle.task is None:
        # Built but never started serving (mounted, then stopped before boot
        # finished) — nothing to drain.
        return True
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


def _browser_reachable(web_id: str, kernel) -> list[str]:
    """Which agents a browser could actually fetch through this host, right now.

    Asks the REAL gates rather than re-deriving them: `authorize_call` for both
    directions (the same engine `Agent.send` uses) and the io leg's own resolved
    ingress rule (the same one `file_bridge` consults). A reimplementation here
    would be a second truth that silently drifts — which is the exact bug this
    field exists to expose."""
    from io_bridge import gate_inbound, resolve_ingress
    from kernel._authz import authorize_call

    web = kernel.ctx.agents.get(web_id)
    if web is None:
        return []
    out: list[str] = []
    for aid, agent in kernel.ctx.agents.items():
        if aid == web_id:
            continue
        rec = kernel.get(aid) or {}
        # An agent that cannot answer the SOURCE verb is not a file server at all.
        servable = "read_stream" in (
            rec.get("verbs") or getattr(agent, "verbs", {}) or {}
        ) or str(rec.get("handler_module", "")).startswith("file_bridge")
        if not servable:
            continue
        egress = authorize_call(
            web.record.get("can_call"),
            "read_stream",
            aid,
            holder_id=web_id,
            holder_parent=web.record.get("parent_id"),
            holder_children=frozenset(getattr(web, "_children", {}) or {}),
        )
        # the heritable half — an ancestor's `{aid: {verbs, for: descendants}}`
        if not egress.allowed and not web._heritable_egress(web_id, "read_stream", aid):
            continue
        ingress = authorize_call(
            rec.get("callable_by"),
            "read_stream",
            web_id,
            holder_id=aid,
            holder_parent=rec.get("parent_id"),
            holder_children=frozenset(getattr(agent, "_children", {}) or {}),
            in_subtree=web._in_subtree,
        )
        if not ingress.allowed:
            continue
        leg = gate_inbound(
            resolve_ingress(rec),
            "call",
            {"target": aid, "payload": {"type": "read_stream"}},
        )
        if leg.allowed:
            out.append(aid)
    return sorted(out)


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
        "host": rec.get("host") or DEFAULT_HOST,
        "allowed_origins": sorted(own_origins(rec))
        + [o for o in extra_origins(rec) if o not in own_origins(rec)],
        "running": id in _servers,
        "served_agent_count": len(kernel.list()),
        "surfaces": surfaces,
        # THE BROWSER-FACING CONTRACT. `/file/` is the only route a browser hits,
        # and it was described NOWHERE — not here, not in the readme. Measured
        # 2026-08-21: a model reflected this agent, found no route contract, and
        # HALLUCINATED one — inventing a `routes` key and the rule "the agent must
        # be in can_call AND expose read_stream", which omits the ingress half — then
        # verified against its own fiction. Six runs across two models opened a
        # public directory's io leg, never granted the web host on its call gate,
        # and reported the browser could reach it. The field they invented is the
        # field that was missing.
        "routes": {
            "GET /<agent_id>/file/<path>": (
                "Streams a file by read_stream-ing through the named agent. The "
                "read is dispatched AS THIS WEB AGENT, so it needs BOTH sides of "
                "the call gate — this host's can_call[<agent_id>] (egress) AND that "
                "agent's callable_by[<this web id>] (ingress) — plus that agent's "
                "own io leg open (ingress_rule). Any one missing → 404. See "
                "browser_reachable for who actually passes all three."
            )
        },
        # The PROPERTY, computed — not a description of it. Every model so far
        # "verified" its work by re-reading config, and config did not carry the
        # answer: `ingress_rule=allow_all_ingress` reads like "open" while the call gate
        # still refuses. This answers "what can a browser actually get?" in the
        # one round-trip they already make.
        "browser_reachable": _browser_reachable(id, kernel),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, kernel):
    """Idempotent. Builds the host on rec.port (required, no default), mounts each
    child's call-surface routes via duck-typed `get_routes`, THEN starts uvicorn —
    so the port never serves a request before its routes are mounted. Returns
    {running:true, surfaces:[...]}."""
    if id not in _servers:
        await _spawn(id, kernel)
    handle = _servers.get(id)
    if handle is None:
        return {"running": False}
    # Mount all child surfaces onto the app BEFORE the listener starts accepting.
    await _mount_all_surfaces(handle, kernel, id)
    _start_serving(id, handle)
    # Announce OURSELVES to the cli renderer (the boot-event convention): the web
    # host owns its port, so it tells the terminal where to attach — the renderer
    # never reaches in for it. No cli (non-tty) ⇒ nobody is told; the bound port
    # is on this agent's reflect. The URL names the BOUND host (uvicorn's config)
    # — a host bound to a LAN address does not answer at 127.0.0.1 (audit ARCH2-10).
    line = (
        f"listening on http://{handle.server.config.host}:{handle.port}/"
        " · rest/ws are children (reflect kernel for ids)"
    )
    await kernel.send(
        [
            {
                "target": "cli",
                "payload": {"type": "say", "source": "web", "text": line},
            }
        ]
    )
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


# Shutdown frees the same process resources — declared, never inferred.
on_shutdown = on_delete
