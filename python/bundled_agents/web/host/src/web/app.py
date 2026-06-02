"""FastAPI factory for one web agent (rendering host).

Routes baked into make_app:
  GET  /                          -> agent index (HTML)
  GET  /_assets/favicon.png       -> bundled favicon (+ /favicon.png fallback)
  GET  /{agent_id}/file/{path}    -> proxy to agent's `read` verb (static alias)

The web host does exactly two things: serve STATIC files through the `file`
alias above, and carry `send()` calls + events over the WS bus (the `web_ws`
sub-agent). It renders no agent UI server-side — frontend panels live in the TS
kernel and render there; this host just serves the static `dist` (via a `file`
agent) and relays the bus. See `ts/SERVE.md`.

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
from fastapi.responses import HTMLResponse, Response


async def _index_page(kernel) -> str:
    """Render the root index — the substrate tree. Reads the HTML scaffold per
    request so editing `templates/index.html` hot-reloads (matches the
    edit-and-refresh dev loop other webapps use)."""
    primer = await kernel.send("kernel", {"type": "reflect"})
    tree = primer.get("tree", {})

    def _esc(s: str) -> str:
        return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")

    def _render(node: dict, depth: int = 0) -> str:
        aid = node["id"]
        name = node.get("display_name") or aid
        hm = node.get("handler_module") or "(root)"
        children = node.get("children", [])
        kids = (
            "<ul>" + "".join(_render(c, depth + 1) for c in children) + "</ul>"
            if children
            else ""
        )
        return (
            f'<li><span class="id">{_esc(name)}</span>'
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

    return app
