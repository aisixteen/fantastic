"""
FastAPI server — WebSocket for frontend, REST for external callers.

Universal dispatch: WS message types map directly to _DISPATCH tool names.
The "type" field IS the tool name; remaining fields are kwargs.

All API/WS endpoints stay at root paths (/api/*, /ws).
Plugin-specific routes registered via hooks.
"""

import json
import logging
import mimetypes
from typing import Any

import httpx
from dotenv import load_dotenv
from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse
from starlette.requests import Request
from starlette.responses import Response
from starlette.routing import Mount, Route

from .._paths import env_path, bundled_agents_dir
from . import _state
from ._ws import ws_subscriptions
from ._broadcast_mode import broadcast_viewers

logger = logging.getLogger(__name__)

# Load .env from project root
_env = env_path()
if _env:
    load_dotenv(_env)


def _resolve_message_scope(message: dict[str, Any]) -> str:
    """Determine broadcast scope via registered resolvers."""
    for resolver in _state._broadcast_resolvers:
        result = resolver(message)
        if result:
            return result
    return ""


async def broadcast(message: dict[str, Any]) -> None:
    """Send a message to subscribed frontend clients and broadcast viewers."""
    logger.debug(f"WS -> {message.get('type', '?')} to {len(ws_subscriptions)} clients")
    data = json.dumps(message, default=str)
    dead = set()
    msg_scope = _resolve_message_scope(message)
    for ws, sub_scope in ws_subscriptions.items():
        if sub_scope and msg_scope and sub_scope != msg_scope:
            continue
        try:
            await ws.send_text(data)
        except Exception:
            dead.add(ws)
    for ws in dead:
        ws_subscriptions.pop(ws, None)
    # Broadcast viewers get everything (readonly mirror)
    if broadcast_viewers:
        dead_v = set()
        for ws in broadcast_viewers:
            try:
                await ws.send_text(data)
            except Exception:
                dead_v.add(ws)
        broadcast_viewers.difference_update(dead_v)


# Import after broadcast is defined (modules use it)
from ._lifespan import lifespan  # noqa: E402
from ._ws import websocket_endpoint  # noqa: E402
from ._rest import (  # noqa: E402
    resolve_agent,
    execute_agent,
    get_state,
    list_files_rest,
    get_handbook_rest,
    api_call_proxy,
    api_schema,
    favicon_redirect,
    serve_content_alias,
    get_agent_memory,
    post_agent_memory,
)
from ._broadcast_mode import (  # noqa: E402
    broadcast_viewer_ws,
    start_broadcast,
    stop_broadcast,
    broadcast_status,
)

app = FastAPI(title="Fantastic", version="1.0.0", lifespan=lifespan)

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)


# ─── Bundle serving ──────────────────────────────────────────────────────


@app.get("/bundles/{name}/{path:path}")
async def serve_bundle(name: str, path: str):
    """Serve bundle assets — user overrides, built-in, or installed plugins."""
    # 1. User override: .fantastic/bundles/{name}/{path}
    if _state.engine:
        user_path = _state.engine.project_dir / ".fantastic" / "bundles" / name / path
        if user_path.exists() and user_path.is_file():
            mt, _ = mimetypes.guess_type(str(user_path))
            return FileResponse(
                str(user_path), media_type=mt or "application/octet-stream"
            )

    # 2. Built-in: bundled_agents/{name}/dist/{path}
    builtin = bundled_agents_dir() / name / "dist" / path
    if builtin.exists() and builtin.is_file():
        mt, _ = mimetypes.guess_type(str(builtin))
        return FileResponse(str(builtin), media_type=mt or "application/octet-stream")

    # 3. Installed plugin: .fantastic/plugins/{name}/dist/{path}
    if _state.engine:
        plugin_asset = (
            _state.engine.project_dir / ".fantastic" / "plugins" / name / "dist" / path
        )
        if plugin_asset.exists() and plugin_asset.is_file():
            mt, _ = mimetypes.guess_type(str(plugin_asset))
            return FileResponse(
                str(plugin_asset), media_type=mt or "application/octet-stream"
            )

    return Response(status_code=404, content=f"Bundle asset not found: {name}/{path}")


# ─── Port proxy — forward HTTP+WS to localhost:{port} ─────────────────────

_proxy_client = httpx.AsyncClient(timeout=30.0)


@app.api_route(
    "/proxy/{port}/{path:path}",
    methods=["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD"],
)
async def proxy_http(port: int, path: str, request: Request):
    """Reverse proxy HTTP requests to localhost:{port}."""
    target = f"http://127.0.0.1:{port}/{path}"
    if request.query_params:
        target += f"?{request.query_params}"
    body = await request.body()
    headers = dict(request.headers)
    headers.pop("host", None)
    # X-Forwarded headers for reverse proxy support (VSCode PR #237411)
    headers["X-Forwarded-Prefix"] = f"/proxy/{port}"
    headers["X-Forwarded-Port"] = str(request.url.port or 80)
    headers["X-Forwarded-Host"] = request.headers.get("host", "localhost")
    resp = await _proxy_client.request(
        request.method,
        target,
        content=body,
        headers=headers,
    )
    return Response(
        content=resp.content,
        status_code=resp.status_code,
        headers=dict(resp.headers),
    )


@app.websocket("/proxy/{port}/{path:path}")
async def proxy_ws(port: int, path: str, ws: WebSocket):
    """Reverse proxy WebSocket to localhost:{port}."""
    import asyncio
    import websockets

    await ws.accept()
    target = f"ws://127.0.0.1:{port}/{path}"
    try:
        async with websockets.connect(target) as remote:

            async def client_to_remote():
                try:
                    while True:
                        data = await ws.receive_text()
                        await remote.send(data)
                except WebSocketDisconnect:
                    pass

            async def remote_to_client():
                try:
                    async for msg in remote:
                        await ws.send_text(msg)
                except Exception:
                    pass

            await asyncio.gather(client_to_remote(), remote_to_client())
    except Exception:
        pass
    finally:
        try:
            await ws.close()
        except Exception:
            pass


# ─── Core route registration ─────────────────────────────────────────────

app.websocket("/ws")(websocket_endpoint)
app.websocket("/ws/broadcast")(broadcast_viewer_ws)

app.post("/api/agents/{agent_id}/resolve")(resolve_agent)
app.post("/api/agents/{agent_id}/execute")(execute_agent)

app.get("/api/state")(get_state)
app.get("/api/files")(list_files_rest)
app.get("/api/handbook")(get_handbook_rest)
app.post("/api/broadcast/start")(start_broadcast)
app.post("/api/broadcast/stop")(stop_broadcast)
app.get("/api/broadcast/status")(broadcast_status)
app.post("/api/call")(api_call_proxy)
app.get("/api/schema")(api_schema)
app.get("/api/agents/{agent_id}/memory")(get_agent_memory)
app.post("/api/agents/{agent_id}/memory")(post_agent_memory)
app.get("/favicon.ico")(favicon_redirect)
app.get("/content/{alias_id}")(serve_content_alias)


def mount_all_apps():
    """Mount plugin routes. Called from lifespan after init_tools()."""
    for hook in _state._route_hooks:
        hook(app, _state)


def remount_web_ui():
    """Hot-swap plugin routes (called when bundles are added/removed)."""
    # Remove existing plugin mounts and root page
    app.routes[:] = [
        r
        for r in app.routes
        if not (
            (
                isinstance(r, Mount)
                and getattr(r, "name", None) == "static"
                and r.path == ""
            )
            or (
                isinstance(r, Route)
                and getattr(r, "name", None) in ("default_shell", "root_page")
            )
        )
    ]
    mount_all_apps()
