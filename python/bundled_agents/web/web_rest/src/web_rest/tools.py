"""web_rest — HTTP/REST verb-invocation surface as a sub-agent of `web`.

Diagnostic-friendly: any curl-able client can POST a verb payload to
`/<self_id>/<target_id>` and get the kernel.send reply back as JSON.
Multiple instances coexist with different ids (and, later, different
auth/logging knobs on the record).

Routes mounted on the parent web's FastAPI app:

    POST /<self_id>/<target_id>       body: JSON payload (must contain `type`)
                                      → JSON reply from kernel.send
    GET  /<self_id>/_reflect          → kernel.reflect (substrate primer)
    GET  /<self_id>/_reflect/<target> → reflect on a specific agent

The GET routes are browser-pastable shortcuts for the universal
discovery verb. They're typed (single verb, no body, idempotent) so
they don't slide into a generic HTTP call channel — POST is still the
only path for arbitrary verbs.

The `self_id` is baked into the path literal so several `web_rest`
agents under the same `web` parent don't collide.

Verbs:
  reflect       -> identity + URL patterns
  boot          -> no-op (web mounts the route by pulling get_routes)
  get_routes    -> the duck-typed call surface used by `web._boot`
"""

from __future__ import annotations

import json

from fastapi import Request
from fastapi.responses import JSONResponse, Response

from kernel import _current_sender


# ─── route endpoints ────────────────────────────────────────────


def _make_post_endpoint(self_id: str, kernel):
    async def _rest_call(request: Request, target_id: str):
        body = await request.body()
        try:
            payload = json.loads(body) if body else {}
        except json.JSONDecodeError:
            # Don't echo the parser exception back — generic message
            # only (CodeQL: information exposure through an exception).
            return JSONResponse(
                {"error": "web_rest: request body is not valid JSON"},
                status_code=400,
            )
        if not isinstance(payload, dict):
            return JSONResponse(
                {"error": "web_rest: body must be a JSON object"}, status_code=400
            )
        # Tag the dispatch with this surface's id so telemetry rays
        # originate visually from this sprite. Without this an external
        # HTTP caller has no agent context and rays drop.
        token = _current_sender.set(self_id)
        try:
            reply = await kernel.send(target_id, payload)
        finally:
            _current_sender.reset(token)
        if reply is None:
            return Response(status_code=204)
        return JSONResponse(reply)

    return _rest_call


def _reflect_payload(request: Request) -> dict:
    """Reflect payload, honoring the `?readme=1` query flag so the
    browser-pastable GET shortcuts can pull an agent's readme too."""
    payload: dict = {"type": "reflect"}
    if request.query_params.get("readme") in ("1", "true", "yes"):
        payload["return_readme"] = True
    return payload


def _make_reflect_get(self_id: str, kernel):
    async def _reflect_target(request: Request, target_id: str):
        token = _current_sender.set(self_id)
        try:
            reply = await kernel.send(target_id, _reflect_payload(request))
        finally:
            _current_sender.reset(token)
        if reply is None:
            return Response(status_code=404)
        return JSONResponse(reply)

    return _reflect_target


def _make_reflect_root(self_id: str, kernel):
    async def _reflect_kernel(request: Request):
        token = _current_sender.set(self_id)
        try:
            reply = await kernel.send("kernel", _reflect_payload(request))
        finally:
            _current_sender.reset(token)
        if reply is None:
            return Response(status_code=404)
        return JSONResponse(reply)

    return _reflect_kernel


# Alias kept for tests that import the legacy endpoint factory name.
_make_endpoint = _make_post_endpoint


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + URL patterns + curl examples. No args."""
    return {
        "id": id,
        "sentence": "REST verb-invocation surface; POST /<self>/<target_id> body=payload.",
        "path_pattern": f"/{id}/{{target_id}}",
        "method": "POST",
        "reflect_url": f"/{id}/_reflect",
        "reflect_pattern": f"/{id}/_reflect/{{target_id}}",
        "curl_post": (
            f'curl -X POST -H "content-type: application/json" '
            f'-d \'{{"type":"reflect"}}\' http://<host>/{id}/<target_id>'
        ),
        "curl_reflect": f"curl http://<host>/{id}/_reflect           # substrate primer",
        "curl_reflect_target": f"curl http://<host>/{id}/_reflect/<target_id>  # any agent",
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, kernel):
    """No-op. The parent `web` agent calls `get_routes` on this bundle during its own boot and mounts the HTTP endpoint onto its FastAPI app."""
    return None


async def _get_routes(id, payload, kernel):
    """Declares this surface's HTTP routes. Returns
    {routes:[…]}. Each spec is `(request, target_id) -> Response` shape
    that FastAPI expects for `app.add_api_route` / `add_api_websocket_route`."""
    return {
        "routes": [
            {
                "kind": "http",
                "method": "POST",
                "path": f"/{id}/{{target_id}}",
                "endpoint": _make_post_endpoint(id, kernel),
            },
            # GET shortcut: reflect kernel (substrate primer). No target
            # in URL — default. Browser-pastable.
            {
                "kind": "http",
                "method": "GET",
                "path": f"/{id}/_reflect",
                "endpoint": _make_reflect_root(id, kernel),
            },
            # GET shortcut: reflect a specific agent. Browser-pastable.
            {
                "kind": "http",
                "method": "GET",
                "path": f"/{id}/_reflect/{{target_id}}",
                "endpoint": _make_reflect_get(id, kernel),
            },
        ]
    }


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "get_routes": _get_routes,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"web_rest: unknown type {t!r}"}
    return await fn(id, payload, kernel)
