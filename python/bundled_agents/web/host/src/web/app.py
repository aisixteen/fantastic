"""FastAPI factory for one web agent (rendering host).

Routes baked into make_app:
  GET  /                          -> agent index (HTML)
  GET  /_assets/favicon.png       -> bundled favicon (+ /favicon.png fallback)
  GET  /{agent_id}/file/{path}    -> proxy to agent's `read_stream` (SOURCE; streamed)

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

import mimetypes
from importlib import resources

from fastapi import FastAPI
from fastapi.responses import HTMLResponse, Response, StreamingResponse


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
        # `/` is the agent-tree index. A custom landing page is NOT a web concern —
        # serve it like any other file, through the gated `/file/` route via a
        # file_bridge the operator wires (web does no direct disk IO of its own).
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
        """Static-file proxy: any agent answering the SOURCE stream verb
        `read_stream{path,offset,length}` becomes an HTTP file server. file_bridge
        is the canonical implementer. URL convention:
        `<img src="/<file_bridge>/file/imgs/foo.png">` works without registration.
        The serving ALLOWANCE is the agent's own gate — a sealed file_bridge's
        `read_stream` denies, so the URL 404s; the path stays clamped to its root.

        ONE contract (no fallback): the file is piped chunk-by-chunk over
        `read_stream` so a LARGE file never loads whole into memory. An agent that
        wants to be HTTP-file-served implements `read_stream`; anything that doesn't
        answer it with a chunk (denied gate, path escape, missing file, non-server)
        is a 404."""
        if not kernel.get(agent_id):
            return Response(status_code=404)
        # Streaming path: read_stream chunk 0 doubles as the gate + size probe.
        first = await kernel.send(
            agent_id,
            {"type": "read_stream", "path": path, "offset": 0, "length": 262144},
        )
        if isinstance(first, dict) and isinstance(
            first.get("bytes"), (bytes, bytearray)
        ):
            size = int(first.get("size", 0))
            mime, _ = mimetypes.guess_type(path)

            async def _stream():
                chunk = first
                while True:
                    yield bytes(chunk["bytes"])
                    if chunk.get("eof"):
                        break
                    chunk = await kernel.send(
                        agent_id,
                        {
                            "type": "read_stream",
                            "path": path,
                            "offset": chunk["next_offset"],
                            "length": 262144,
                        },
                    )
                    if not isinstance(chunk, dict) or not isinstance(
                        chunk.get("bytes"), (bytes, bytearray)
                    ):
                        break

            return StreamingResponse(
                _stream(),
                media_type=mime or "application/octet-stream",
                headers={"content-length": str(size)} if size else None,
            )
        # Not a SOURCE (denied gate, path escape, missing file, non-file-server):
        # 404. No whole-file `read` fallback — read_stream is the one contract.
        return Response(status_code=404)

    return app
