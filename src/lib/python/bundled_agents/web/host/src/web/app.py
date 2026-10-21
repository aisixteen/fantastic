"""FastAPI factory for one web agent (HTTP host).

Routes baked into make_app:
  GET  /_assets/favicon.png       -> bundled favicon
  GET  /{agent_id}/file/{path}    -> proxy to agent's `read_stream` (SOURCE; streamed)

There is no `/` page: the host renders nothing of its own, so an undeclared `/`
is a 404. A landing page is a declared route or a file served through the file
route like any other.

The web host does exactly two things: serve STATIC files through the file
route above, and carry `send()` calls + events over the WS bus (the `web_ws`
sub-agent). It renders no agent UI server-side — frontend html agents live in the TS
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
from fastapi.responses import Response, StreamingResponse


#: The web host binds LOOPBACK unless its record names a `host` — a closed default,
#: overridable by whoever creates the kernel (create_agent meta, a roster yaml,
#: update_agent). `0.0.0.0` is an explicit choice, never a fallback.
DEFAULT_HOST = "127.0.0.1"

_SAFE_METHODS = frozenset({"GET", "HEAD", "OPTIONS"})

#: One `read_stream` chunk of the file route.
FILE_CHUNK = 262144


def own_origins(rec: dict) -> set[str]:
    """The origins that ARE this web host: `http://<host>:<port>`, plus both loopback
    spellings when it binds loopback. A browser Origin is compared against these (and
    the record's `allowed_origins`) EXACTLY — never against the request's Host header,
    which a DNS-rebinding page controls."""
    host = rec.get("host") or DEFAULT_HOST
    port = rec.get("port")
    out = {f"http://{host}:{port}"}
    if host in ("127.0.0.1", "localhost", "::1"):
        out |= {
            f"http://127.0.0.1:{port}",
            f"http://localhost:{port}",
            f"http://[::1]:{port}",
        }
    return out


def own_authorities(rec: dict) -> set[str]:
    """The Host values that ARE this web host: `<host>:<port>` (an IPv6 literal in
    brackets), plus the three loopback spellings when it binds loopback. Nothing else
    — a container port map, a LAN name, a tunnel name are other authorities and are
    refused; a `0.0.0.0` bind answers only `0.0.0.0:<port>`."""
    host = rec.get("host") or DEFAULT_HOST
    port = rec.get("port")
    out = {f"[{host}]:{port}" if ":" in host else f"{host}:{port}"}
    if host in ("127.0.0.1", "localhost", "::1"):
        out |= {f"127.0.0.1:{port}", f"localhost:{port}", f"[::1]:{port}"}
    return out


def origin_allowed(rec: dict, origin: str | None) -> bool:
    """No Origin header ⇒ a non-browser client (CLI, curl, a bridge): allowed — the leg's
    own `ingress_rule` still gates it. A browser Origin must be the host's own or listed
    in the record's `allowed_origins` (exact strings)."""
    if not origin:
        return True
    return origin in own_origins(rec) or origin in set(extra_origins(rec))


def extra_origins(rec: dict) -> list[str]:
    """The record's `allowed_origins` as a list — a bare string (what `k=v` on the CLI
    yields) is ONE origin, never its characters."""
    extra = rec.get("allowed_origins") or []
    return [extra] if isinstance(extra, str) else list(extra)


class _OriginGuard:
    """ASGI guard in front of every route of this host, two checks:
    - HOST: EVERY request — GET/HEAD/OPTIONS, unsafe methods and the
      websocket upgrade alike — whose `Host` header is not one of `own_authorities`
      is refused. A DNS-rebinding page sends a same-origin GET with no `Origin`, so
      the Origin check never sees it; its `Host` still names the attacker's domain.
    - ORIGIN: a websocket upgrade, or an HTTP request that is not GET/HEAD/OPTIONS,
      from a FOREIGN browser origin is refused.
    Either refusal is 403 with a coded body on HTTP, a refused upgrade (403) on a
    websocket, and logged. Reads the record per request, so `host`/`port`/
    `allowed_origins` edits apply without a restart."""

    def __init__(self, app, web_agent_id: str, kernel):
        self.app, self.web_agent_id, self.kernel = app, web_agent_id, kernel

    async def _refuse(self, kind, send, error: str, hint: str):
        if kind == "websocket":
            await send({"type": "websocket.close", "code": 1008})
            return
        body = json.dumps(
            {"error": error, "reason": "unauthorized", "hint": hint}
        ).encode()
        await send(
            {
                "type": "http.response.start",
                "status": 403,
                "headers": [(b"content-type", b"application/json")],
            }
        )
        await send({"type": "http.response.body", "body": body})

    async def __call__(self, scope, receive, send):
        kind = scope.get("type")
        if kind in ("http", "websocket"):
            headers = dict(scope.get("headers") or [])
            rec = self.kernel.get(self.web_agent_id) or {}
            host = headers.get(b"host", b"").decode("latin-1")
            if host not in own_authorities(rec):
                print(
                    f"  [web] {self.web_agent_id}: refused {kind} {scope.get('path')} "
                    f"for foreign host {host!r}",
                    flush=True,
                )
                await self._refuse(
                    kind,
                    send,
                    f"host {host!r} is not this server's own authority",
                    "address the host by its bound host:port (a loopback bind also "
                    "answers localhost, 127.0.0.1 and [::1] on that port)",
                )
                return
            origin = headers.get(b"origin", b"").decode("latin-1") or None
            gated = (
                kind == "websocket" or scope.get("method", "GET") not in _SAFE_METHODS
            )
            if gated and not origin_allowed(rec, origin):
                print(
                    f"  [web] {self.web_agent_id}: refused {kind} {scope.get('path')} "
                    f"from foreign origin {origin!r}",
                    flush=True,
                )
                await self._refuse(
                    kind,
                    send,
                    f"origin {origin!r} is not allowed on this host",
                    "add it to the web record's allowed_origins",
                )
                return
        await self.app(scope, receive, send)


def make_app(web_agent_id: str, kernel) -> FastAPI:
    # NO GENERATED SURFACES. FastAPI mounts `/docs`, `/redoc` and `/openapi.json`
    # by default — routes nobody declared, answering on every web agent and
    # describing its whole route table to anyone who asks. A web agent serves only
    # what its legs declare.
    app = FastAPI(docs_url=None, redoc_url=None, openapi_url=None)

    _favicon_bytes = (resources.files("web") / "favicon.png").read_bytes()

    @app.get("/_assets/favicon.png")
    async def favicon_asset():
        return Response(_favicon_bytes, media_type="image/png")

    async def _fetch(agent_id: str, path: str, offset: int):
        """One chunk, read BY THE WEB HOST: the request is handed to it as
        `fetch_file` (this route is the transport — the operator), and its handler
        sends `read_stream` as itself."""
        return (
            await kernel.send(
                [
                    {
                        "target": web_agent_id,
                        "payload": {
                            "type": "fetch_file",
                            "agent_id": agent_id,
                            "path": path,
                            "offset": offset,
                            "length": FILE_CHUNK,
                        },
                    }
                ]
            )
        )[0]["result"]

    @app.get("/{agent_id}/file/{path:path}")
    async def agent_file(agent_id: str, path: str):
        """Static-file proxy: any agent answering the SOURCE stream verb
        `read_stream{path,offset,length}` becomes an HTTP file server. file_bridge
        is the canonical implementer. URL convention:
        `<img src="/<file_bridge>/file/imgs/foo.png">` works without registration.
        The serving ALLOWANCE is BOTH of the agent's gates. The request is handed
        to the web host (`fetch_file`, as the transport — the operator) and the
        HOST reads each chunk AS ITSELF — never the operator, which would hand the
        browser the origin root of trust with `callable_by` never consulted — so a
        browser fetch is an ordinary agent send FROM the web host: the served
        agent's `ingress_rule` gates the io leg AND its `callable_by` decides whether
        `web` may call it at all.

        Run as operator, this route would serve a browser a leg whose `callable_by`
        excludes every web surface: "in-kernel readers yes, browser no" would be
        INEXPRESSIBLE for a file_bridge, since `ingress_rule` is one switch over both
        origins. With the route acting as the host it is just `callable_by` without
        `web`.

        ONE contract (no fallback): the file is piped chunk-by-chunk over
        `read_stream` so a LARGE file never loads whole into memory. An agent that
        wants to be HTTP-file-served implements `read_stream`; anything that doesn't
        answer it with a chunk (denied gate, path escape, missing file, non-server)
        is a 404."""
        if not kernel.get(agent_id):
            return Response(status_code=404)
        # Streaming path: read_stream chunk 0 doubles as the gate + size probe.
        first = await _fetch(agent_id, path, 0)
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
                    chunk = await _fetch(agent_id, path, chunk["next_offset"])
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
        # fixes it — and this route keeps it: a bare 404 for every cause alike would
        # make even curling the URL teach nothing (a leg opened with
        # `ingress_rule=allow_all_ingress` but no `web` grant looks like success).
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

    app.add_middleware(_OriginGuard, web_agent_id=web_agent_id, kernel=kernel)
    return app
