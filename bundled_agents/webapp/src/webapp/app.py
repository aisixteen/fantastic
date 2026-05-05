"""FastAPI factory for one web agent.

Routes:
  GET  /                          -> agent index (HTML)
  GET  /_kernel/reflect           -> kernel reflect JSON
  GET  /_agents                   -> list_agents JSON
  GET  /_fantastic/transport.js   -> the inlined transport
  GET  /{agent_id}/               -> bundle's web/index.html (or placeholder)
  POST /{agent_id}/call           -> body becomes payload, kernel.send → JSON
  WS   /{agent_id}/ws             -> proxy.run(ws, kernel, agent_id)
"""

from __future__ import annotations

import base64
import json
import mimetypes

from fastapi import FastAPI, Request, WebSocket
from fastapi.responses import (
    HTMLResponse,
    JSONResponse,
    PlainTextResponse,
    Response,
)

from kernel import _current_sender

from . import _proxy
from ._transport_js import TRANSPORT_JS

_TRANSPORT_TAG = '<script src="/_fantastic/transport.js"></script>'


def _inject(html: str) -> str:
    lower = html.lower()
    idx = lower.find("<head>")
    if idx == -1:
        return _TRANSPORT_TAG + html
    return html[: idx + 6] + "\n  " + _TRANSPORT_TAG + html[idx + 6 :]


def _index_page(agents: list) -> str:
    rows = "\n".join(
        f'<li><a href="/{a["id"]}/">{a["id"]}</a> — <code>{a.get("handler_module", "")}</code></li>'
        for a in agents
    )
    return f"""<!doctype html>
<html><head><title>fantastic</title>
<style>body{{font-family:system-ui;background:#0c0c14;color:#ccc;padding:32px}}
a{{color:#9ad}} code{{background:#222;padding:1px 5px;border-radius:3px;font-size:12px}}
li{{margin:6px 0}}</style></head>
<body>
<h2>fantastic</h2>
<p><a href="/_kernel/reflect">/_kernel/reflect</a> · <a href="/_agents">/_agents</a></p>
<ul>{rows}</ul>
</body></html>"""


def make_app(web_agent_id: str, kernel) -> FastAPI:
    app = FastAPI()

    @app.get("/", response_class=HTMLResponse)
    async def root():
        agents = kernel.list()
        return HTMLResponse(_index_page(agents))

    @app.get("/_fantastic/transport.js", response_class=PlainTextResponse)
    async def transport_js():
        return PlainTextResponse(TRANSPORT_JS, media_type="application/javascript")

    @app.get("/_kernel/reflect")
    async def kernel_reflect(request: Request):
        primer = await kernel.send("kernel", {"type": "reflect"})
        host = request.url.netloc
        # Webapp owns HTTP/WS knowledge; merge it into the substrate primer
        # so a remote caller can bootstrap from one round-trip.
        primer.setdefault("transports", {})
        primer["transports"]["http"] = {
            "agent_call": f"POST http://{host}/<agent_id>/call  body=<payload-json>",
            "kernel_reflect": f"GET http://{host}/_kernel/reflect",
            "agents_list": f"GET http://{host}/_agents",
            "agent_index": f"GET http://{host}/<agent_id>/  (HTML if the bundle ships a webapp)",
            "use_when": "any external caller (curl, fetch, another service).",
        }
        primer["transports"]["ws"] = {
            "url": f"ws://{host}/<agent_id>/ws",
            "text_frame": '{"type":"call","target":"<id>","payload":{...},"id":"<corr>"}',
            "binary_frame": "see top-level `binary_protocol`",
            "frames_in": ["call", "emit", "watch", "unwatch"],
            "frames_out": ["reply", "error", "event"],
            "use_when": "long-lived connection; needed for `watch` (event stream).",
        }
        return JSONResponse(primer)

    @app.get("/_agents")
    async def agents_list():
        r = await kernel.send("core", {"type": "list_agents"})
        return JSONResponse(r)

    @app.get("/{agent_id}/file/{path:path}")
    async def agent_file(agent_id: str, path: str):
        """Static-file proxy: turn any agent that answers `read{path}`
        into an HTTP file server. file_<id> is the canonical implementer.

        Replaces the old `content_alias_file` registry with a URL
        convention: `<img src="/<file_agent>/file/imgs/foo.png">` works
        in any html_agent without registration.
        """
        if not kernel.get(agent_id):
            return Response(status_code=404)
        r = await kernel.send(agent_id, {"type": "read", "path": path})
        if not isinstance(r, dict) or r.get("error"):
            return Response(status_code=404)
        if "image_base64" in r:
            return Response(
                base64.b64decode(r["image_base64"]),
                media_type=r.get("mime", "application/octet-stream"),
            )
        if "content" in r:
            mime, _ = mimetypes.guess_type(path)
            return Response(
                r["content"].encode("utf-8"),
                media_type=mime or "text/plain; charset=utf-8",
            )
        return Response(status_code=404)

    @app.get("/{agent_id}/", response_class=HTMLResponse)
    async def agent_index(agent_id: str):
        """Single presentation protocol: every UI agent answers
        `render_html` with `{html:str}`. We inject transport.js and serve.
        Bundles whose HTML lives in `<package>/webapp/index.html` read it
        themselves inside their own `_render_html` (see e.g.
        terminal_webapp). Backends that have no UI don't implement
        the verb → 404. No bundled-file fallback here.
        """
        if not kernel.get(agent_id):
            return HTMLResponse(f"no agent {agent_id!r}", status_code=404)
        r = await kernel.send(agent_id, {"type": "render_html"})
        if not isinstance(r, dict) or not isinstance(r.get("html"), str):
            return HTMLResponse(
                f"agent {agent_id!r} does not implement render_html",
                status_code=404,
            )
        return HTMLResponse(_inject(r["html"]))

    @app.post("/{agent_id}/call")
    async def agent_call(agent_id: str, request: Request):
        body = await request.body()
        payload = json.loads(body) if body else {}
        # Tag the dispatch with this webapp's id so telemetry rays
        # originate visually from the webapp sprite. Without this an
        # external HTTP caller has no agent context and rays drop.
        token = _current_sender.set(web_agent_id)
        try:
            reply = await kernel.send(agent_id, payload)
        finally:
            _current_sender.reset(token)
        return Response(
            json.dumps(reply, default=str, ensure_ascii=False),
            media_type="application/json",
        )

    @app.websocket("/{agent_id}/ws")
    async def agent_ws(ws: WebSocket, agent_id: str):
        await ws.accept()
        await _proxy.run(ws, kernel, agent_id, web_agent_id)

    return app
