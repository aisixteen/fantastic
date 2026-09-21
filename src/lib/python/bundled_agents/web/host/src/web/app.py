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
via the duck-typed `get_routes` verb (pure data) and hand over the live
endpoints via the duck-typed `route_endpoints(agent)` module hook;
`web.tools._mount_surfaces` mounts both halves onto this app at runtime.
"""

from __future__ import annotations

import json
import mimetypes
from importlib import resources

from fastapi import FastAPI
from kernel import sender_context
from fastapi.responses import HTMLResponse, Response, StreamingResponse


async def _index_page(kernel) -> str:
    """Render the root index — the substrate tree. Reads the HTML scaffold per
    request so editing `templates/index.html` hot-reloads (matches the
    edit-and-refresh dev loop other webapps use)."""
    primer = (
        await kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
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
        The serving ALLOWANCE is BOTH of the agent's gates. This request enters from
        OUTSIDE any handler, so without help `_current_sender` would be None and the
        read would carry OPERATOR authority — the browser inheriting the origin root
        of trust, and `callable_by` never consulted. It is dispatched under
        `sender_context(web_agent_id)` instead, so a browser fetch is an ordinary
        agent send FROM the web host: the served agent's `ingress_rule` gates the io
        leg AND its `callable_by` decides whether `web` may call it at all.

        Measured 2026-08-21 (`live_plan`): with this route running as operator, a
        model set `transcripts` to `ingress_rule=allow_all_ingress` plus a `callable_by`
        excluding every web surface, said "any future web/ws/rest agent are
        deliberately excluded" — and the browser was served the private chat history
        anyway. Its two-sided reasoning was right; the route's origin made it false.
        Before this, "in-kernel readers yes, browser no" was INEXPRESSIBLE for a
        file_bridge: `ingress_rule` is one switch over both origins and `callable_by`
        could not reach the HTTP path. Now it is just `callable_by` without `web`.

        ONE contract (no fallback): the file is piped chunk-by-chunk over
        `read_stream` so a LARGE file never loads whole into memory. An agent that
        wants to be HTTP-file-served implements `read_stream`; anything that doesn't
        answer it with a chunk (denied gate, path escape, missing file, non-server)
        is a 404."""
        if not kernel.get(agent_id):
            return Response(status_code=404)
        # Streaming path: read_stream chunk 0 doubles as the gate + size probe.
        with sender_context(web_agent_id):
            first = (
                await kernel.send(
                    [
                        {
                            "target": agent_id,
                            "payload": {
                                "type": "read_stream",
                                "path": path,
                                "offset": 0,
                                "length": 262144,
                            },
                        }
                    ]
                )
            )[0]["result"]
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
                    # The generator is iterated by StreamingResponse AFTER the
                    # handler returned, so it re-enters the context per chunk
                    # rather than relying on the handler's still being active.
                    with sender_context(web_agent_id):
                        chunk = (
                            await kernel.send(
                                [
                                    {
                                        "target": agent_id,
                                        "payload": {
                                            "type": "read_stream",
                                            "path": path,
                                            "offset": chunk["next_offset"],
                                            "length": 262144,
                                        },
                                    }
                                ]
                            )
                        )[0]["result"]
                    if not isinstance(chunk, dict) or not isinstance(
                        chunk.get("bytes"), (bytes, bytearray)
                    ):
                        break

            return StreamingResponse(
                _stream(),
                media_type=mime or "application/octet-stream",
                headers={"content-length": str(size)} if size else None,
            )
        # Not a SOURCE: 404. No whole-file `read` fallback — read_stream is the one
        # contract.
        #
        # A GATE denial carries its reason in the body. This kernel teaches through
        # denials — every refusal names the `side` and a `hint` with the recipe that
        # fixes it — and this route used to be the one surface that threw that away,
        # returning a bare 404 for every cause alike. Measured 2026-08-21: five runs
        # across two frontier models set `ingress_rule=allow_all_ingress` on a public
        # directory, never granted `web` on its call gate, and reported success;
        # the kernel had said "assets does not accept 'read_stream' from web" and
        # the route discarded it, so even curling the URL taught nothing.
        #
        # A MISSING FILE stays a bare 404 — no body, no existence leak, no path
        # probing. The two cases differ because only one of them is the operator's
        # own misconfiguration, and that one is useless kept secret.
        detail = first if isinstance(first, dict) else {}
        if detail.get("reason") == "unauthorized":
            return Response(
                content=json.dumps(
                    {k: detail.get(k) for k in ("error", "reason", "side", "hint")}
                ),
                status_code=404,
                media_type="application/json",
            )
        return Response(status_code=404)

    return app
