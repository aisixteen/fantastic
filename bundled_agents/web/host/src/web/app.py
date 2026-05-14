"""FastAPI factory for one web agent (rendering host).

Routes baked into make_app:
  GET  /                          -> agent index (HTML)
  GET  /_fantastic/transport.js   -> the inlined transport
  GET  /_assets/favicon.png       -> bundled favicon (+ /favicon.png fallback)
  GET  /{agent_id}/               -> agent's `render_html` page
  GET  /{agent_id}/file/{path}    -> proxy to agent's `read` verb

Call-surface routes (WS, REST) are NOT baked in. They live in
sub-agent bundles (`web_ws`, `web_rest`) that declare their routes
via the duck-typed `get_routes` verb; `web.tools._mount_surfaces`
mounts them onto this app at runtime.
"""

from __future__ import annotations

import base64
import mimetypes
from importlib import resources

from fastapi import FastAPI
from fastapi.responses import (
    HTMLResponse,
    PlainTextResponse,
    Response,
)

from ._transport_js import TRANSPORT_JS

_TRANSPORT_TAG = '<script src="/_fantastic/transport.js"></script>'
_FAVICON_TAG = '<link rel="icon" type="image/png" href="/_assets/favicon.png">'


def _inject(html: str) -> str:
    lower = html.lower()
    idx = lower.find("<head>")
    inject = _TRANSPORT_TAG
    # Inject default favicon link if the page doesn't already declare one.
    if 'rel="icon"' not in lower and "rel='icon'" not in lower:
        inject = _FAVICON_TAG + "\n  " + inject
    if idx == -1:
        return inject + html
    return html[: idx + 6] + "\n  " + inject + html[idx + 6 :]


async def _index_page(kernel) -> str:
    """Render the root index — substrate tree + visit links for any
    agent that serves HTML (answers `render_html` or `get_webapp`).
    Probes each agent in parallel. Reads the HTML scaffold per request
    so editing `templates/index.html` hot-reloads (matches the
    edit-and-refresh dev loop other webapps use)."""
    import asyncio as _asyncio

    primer = await kernel.send("kernel", {"type": "reflect"})
    tree = primer.get("tree", {})

    async def _has_html(agent_id: str) -> bool:
        try:
            r = await kernel.send(agent_id, {"type": "render_html"})
            if isinstance(r, dict) and isinstance(r.get("html"), str):
                return True
        except Exception:
            pass
        try:
            r = await kernel.send(agent_id, {"type": "get_webapp"})
            if isinstance(r, dict) and r.get("url") and not r.get("error"):
                return True
        except Exception:
            pass
        return False

    ids: list[str] = []

    def _collect(node: dict) -> None:
        ids.append(node["id"])
        for c in node.get("children", []):
            _collect(c)

    if tree:
        _collect(tree)
    has_html_results = await _asyncio.gather(*(_has_html(i) for i in ids))
    has_html = dict(zip(ids, has_html_results))

    def _esc(s: str) -> str:
        return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")

    def _render(node: dict, depth: int = 0) -> str:
        aid = node["id"]
        name = node.get("display_name") or aid
        hm = node.get("handler_module") or "(root)"
        visit = (
            f'<a class="visit" href="/{aid}/" title="open agent UI">↗</a>'
            if has_html.get(aid)
            else ""
        )
        children = node.get("children", [])
        kids = (
            "<ul>" + "".join(_render(c, depth + 1) for c in children) + "</ul>"
            if children
            else ""
        )
        return (
            f'<li><span class="id">{_esc(name)}</span>'
            f" {visit}"
            f" <code>{_esc(aid)}</code>"
            f' <span class="hm">{_esc(hm)}</span>{kids}</li>'
        )

    body = _render(tree) if tree else "<li><em>empty tree</em></li>"
    tpl = (resources.files("web") / "templates" / "index.html").read_text("utf-8")
    return tpl.replace("{{tree_body}}", body)


def make_app(web_agent_id: str, kernel) -> FastAPI:
    app = FastAPI()

    @app.get("/", response_class=HTMLResponse)
    async def root():
        return HTMLResponse(await _index_page(kernel))

    @app.get("/_fantastic/transport.js", response_class=PlainTextResponse)
    async def transport_js():
        return PlainTextResponse(TRANSPORT_JS, media_type="application/javascript")

    _favicon_bytes = (resources.files("web") / "favicon.png").read_bytes()

    @app.get("/_assets/favicon.png")
    async def favicon_asset():
        return Response(_favicon_bytes, media_type="image/png")

    @app.get("/favicon.png")
    async def favicon_root():
        return Response(_favicon_bytes, media_type="image/png")

    @app.get("/{agent_id}/file/{path:path}")
    async def agent_file(agent_id: str, path: str):
        """Static-file proxy: any agent answering `read{path}` becomes
        an HTTP file server. file_<id> is the canonical implementer.
        URL convention: `<img src="/<file_agent>/file/imgs/foo.png">`
        works in any html_agent without registration."""
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
        if isinstance(r.get("bytes"), (bytes, bytearray)):
            return Response(
                bytes(r["bytes"]),
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
        """Every UI agent answers `render_html` with `{html:str}`. We
        inject transport.js and serve. Backends without a UI don't
        implement the verb → 404.

        Error bodies are static text and never echo the request-path
        `agent_id` — reflecting untrusted path input into a response
        body is a reflected-XSS sink. The 404 status is the signal;
        the caller already knows the id it requested."""
        if not kernel.get(agent_id):
            return PlainTextResponse("no such agent", status_code=404)
        r = await kernel.send(agent_id, {"type": "render_html"})
        if not isinstance(r, dict) or not isinstance(r.get("html"), str):
            return PlainTextResponse(
                "agent does not implement render_html",
                status_code=404,
            )
        return HTMLResponse(_inject(r["html"]))

    return app
