"""The MCP wire — Streamable HTTP, protocol 2026-07-28 ONLY (owner, 2026-10-08).

Every JSON-RPC request is its own POST to the server's one endpoint, carrying its
version, identity and capabilities in `params._meta` (no handshake, no session) and
mirrored into headers: `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` (a
`tools/call`'s tool), `Mcp-Param-<name>` (each `x-mcp-header` argument). The answer is
one JSON object or an SSE stream scoped to the request (its notifications, then the
response). A server on an older protocol is refused naming its versions — no
handshake fallback.

Every failure is a reply the caller reads: `McpError.reply` is a refusal from the
kernel's closed set, or `{error, mcp_error}` for a JSON-RPC error the server raised.
"""

from __future__ import annotations

import base64
import itertools
import json
from typing import Any, Awaitable, Callable

import httpx
from kernel._refusal import (
    BAD_ARG,
    NOT_CONNECTED,
    TRANSPORT_CLOSED,
    TRANSPORT_ERROR,
    UNAUTHORIZED,
    UNHANDLED,
    refusal,
)

PROTOCOL = "2026-07-28"
CLIENT_INFO = {"name": "aisixteen-fantastic", "version": "0.1.0"}
#: A connect that does not happen fails; a call itself is bounded by its caller's
#: own item timeout (the kernel never defaults one), so there is no read clock.
CONNECT_TIMEOUT = 15.0
#: The transport the client uses — None is the network; tests put an
#: `httpx.MockTransport` here.
TRANSPORT: httpx.AsyncBaseTransport | None = None
#: JSON-RPC result types this client reads (`resultType`; absent = complete).
RESULT_TYPES = ("complete", "input_required")
#: Characters of an HTTP field-name token (RFC 9110 `tchar`).
_TCHAR = set(
    "!#$%&'*+-.^_`|~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
)
_SAFE_INT = 2**53 - 1

_ids = itertools.count(1)


class McpError(Exception):
    """A call that did not produce a result — `reply` is what the caller gets."""

    def __init__(self, reply: dict):
        super().__init__(reply.get("error"))
        self.reply = reply


# ─── header values ──────────────────────────────────────────────────


def header_value(v: str) -> str:
    """A value as an MCP header carries it: as is when it is plain visible ASCII
    (spaces inside), else the `=?base64?…?=` sentinel of its UTF-8 — and a plain
    value shaped like the sentinel is encoded too, so it cannot be misread."""
    plain = (
        v == v.strip(" \t")
        and all(c in " \t" or "!" <= c <= "~" for c in v)
        and not (v.startswith("=?base64?") and v.endswith("?="))
    )
    if plain:
        return v
    return "=?base64?" + base64.b64encode(v.encode("utf-8")).decode("ascii") + "?="


def mirrors(schema: Any) -> list[tuple[str, list[str]]] | str:
    """The `x-mcp-header` annotations of a tool's `inputSchema` → `[(name, path)]`,
    or why the tool must be excluded. Legal only on a primitive property
    (integer/string/boolean) reached through `properties` keys alone; names are
    non-empty RFC 9110 tokens, unique ignoring case."""
    found: list[tuple[str, list[str]]] = []

    def walk_props(node: Any, path: list[str]) -> None:
        props = node.get("properties") if isinstance(node, dict) else None
        if not isinstance(props, dict):
            return
        for key, sub in props.items():
            if isinstance(sub, dict) and "x-mcp-header" in sub:
                found.append((sub["x-mcp-header"], [*path, key]))
            walk_props(sub, [*path, key])

    def count_all(node: Any) -> int:
        if isinstance(node, dict):
            return ("x-mcp-header" in node) + sum(count_all(v) for v in node.values())
        if isinstance(node, list):
            return sum(count_all(v) for v in node)
        return 0

    walk_props(schema, [])
    if count_all(schema) != len(found):
        return "an x-mcp-header outside a chain of `properties` keys"
    seen: set[str] = set()
    for name, path in found:
        if not isinstance(name, str) or not name or not set(name) <= _TCHAR:
            return f"x-mcp-header {name!r} is not an HTTP header token"
        if name.lower() in seen:
            return f"x-mcp-header {name!r} is not unique"
        seen.add(name.lower())
        node = schema
        for key in path:
            node = node["properties"][key]
        if node.get("type") not in ("integer", "string", "boolean"):
            return f"x-mcp-header {name!r} is on a {node.get('type')!r} property — only integer, string or boolean"
    return found


def param_headers(found: list[tuple[str, list[str]]], arguments: dict) -> dict:
    """`Mcp-Param-<name>` for each mirrored argument present (and not null)."""
    out: dict[str, str] = {}
    for name, path in found:
        node: Any = arguments
        for key in path:
            node = node.get(key) if isinstance(node, dict) else None
        if node is None:
            continue
        if isinstance(node, bool):
            text = "true" if node else "false"
        elif isinstance(node, int):
            if abs(node) > _SAFE_INT:
                raise McpError(
                    refusal(
                        BAD_ARG,
                        f"{'.'.join(path)}={node} is outside the integers an MCP header carries",
                        arg=path[0],
                    )
                )
            text = str(node)
        else:
            text = str(node)
        out[f"Mcp-Param-{name}"] = header_value(text)
    return out


# ─── one request ────────────────────────────────────────────────────


def _meta() -> dict:
    return {
        "io.modelcontextprotocol/protocolVersion": PROTOCOL,
        "io.modelcontextprotocol/clientInfo": CLIENT_INFO,
        "io.modelcontextprotocol/clientCapabilities": {},
    }


def _protocol_error(url: str, err: Any) -> McpError:
    """A JSON-RPC error the server answered, as the caller reads it."""
    if not isinstance(err, dict):
        return McpError(
            refusal(TRANSPORT_ERROR, f"mcp {url}: a malformed error {err!r}")
        )
    code, msg = err.get("code"), str(err.get("message", ""))
    data = err.get("data")
    if code == -32602:
        return McpError(refusal(BAD_ARG, f"mcp {url}: {msg}", mcp_code=code))
    if code == -32601:
        return McpError(refusal(UNHANDLED, f"mcp {url}: {msg}", mcp_code=code))
    if code == -32022:
        supported = data.get("supported") if isinstance(data, dict) else None
        return McpError(
            refusal(
                TRANSPORT_ERROR,
                f"mcp {url}: the server speaks {supported} — this client speaks "
                f"{PROTOCOL} only",
                mcp_code=code,
            )
        )
    if code == -32021:
        need = data.get("requiredCapabilities") if isinstance(data, dict) else None
        return McpError(
            refusal(
                TRANSPORT_ERROR,
                f"mcp {url}: the server needs client capabilities {need} this client "
                "does not offer",
                mcp_code=code,
            )
        )
    if code == -32020:
        return McpError(
            refusal(
                TRANSPORT_ERROR,
                f"mcp {url}: header mismatch — {msg}",
                hint="the tool's inputSchema may have changed: boot re-lists the tools",
                mcp_code=code,
            )
        )
    return McpError(
        {"error": f"mcp {url}: error {code}: {msg}", "mcp_error": dict(err)}
    )


def _settle(url: str, msg: Any) -> dict:
    if not isinstance(msg, dict):
        raise McpError(refusal(TRANSPORT_ERROR, f"mcp {url}: not a JSON-RPC message"))
    if "error" in msg:
        raise _protocol_error(url, msg["error"])
    result = msg.get("result")
    if not isinstance(result, dict):
        raise McpError(
            refusal(TRANSPORT_ERROR, f"mcp {url}: a response with no result")
        )
    rtype = result.get("resultType", "complete")
    if rtype not in RESULT_TYPES:
        raise McpError(
            refusal(TRANSPORT_ERROR, f"mcp {url}: unknown resultType {rtype!r}")
        )
    return result


async def rpc(
    url: str,
    method: str,
    params: dict,
    *,
    key: str | None,
    headers: dict | None = None,
    on_note: Callable[[dict], Awaitable[None]] | None = None,
) -> dict:
    """POST one request; → its `result`, or raise `McpError`. Notifications on an SSE
    answer go to `on_note` as they arrive."""
    rid = str(next(_ids))
    body = {
        "jsonrpc": "2.0",
        "id": rid,
        "method": method,
        "params": {**params, "_meta": _meta()},
    }
    head = {
        "Accept": "application/json, text/event-stream",
        "Content-Type": "application/json",
        "MCP-Protocol-Version": PROTOCOL,
        "Mcp-Method": method,
    }
    if method == "tools/call":
        head["Mcp-Name"] = header_value(params["name"])
    head.update(headers or {})
    if key is not None:
        head["Authorization"] = f"Bearer {key}"
    try:
        async with httpx.AsyncClient(
            timeout=httpx.Timeout(None, connect=CONNECT_TIMEOUT), transport=TRANSPORT
        ) as client:
            async with client.stream("POST", url, json=body, headers=head) as resp:
                return await _read(url, rid, resp, key, on_note)
    except (httpx.ConnectError, httpx.ConnectTimeout) as e:
        raise McpError(refusal(NOT_CONNECTED, f"mcp {url}: cannot connect: {e}")) from e
    except httpx.HTTPError as e:
        raise McpError(refusal(TRANSPORT_ERROR, f"mcp {url}: {e}")) from e


async def _read(url, rid, resp: httpx.Response, key, on_note) -> dict:
    status = resp.status_code
    if status in (401, 403):
        why = "refused the key" if key is not None else "needs a key"
        raise McpError(
            refusal(
                UNAUTHORIZED,
                f"mcp {url}: HTTP {status} — the server {why}",
                hint="the key is a credential-store ref: credentials={store, ref}, "
                "field api_key",
                http_status=status,
            )
        )
    ctype = resp.headers.get("content-type", "").split(";")[0].strip().lower()
    if status >= 400:
        raw = await resp.aread()
        try:
            msg = json.loads(raw)
        except ValueError:
            msg = None
        if isinstance(msg, dict) and isinstance(msg.get("error"), dict):
            raise _protocol_error(url, msg["error"])
        text = raw.decode("utf-8", "replace")[:300]
        raise McpError(
            refusal(
                TRANSPORT_ERROR, f"mcp {url}: HTTP {status}: {text}", http_status=status
            )
        )
    if ctype == "application/json":
        try:
            msg = json.loads(await resp.aread())
        except ValueError as e:
            raise McpError(
                refusal(TRANSPORT_ERROR, f"mcp {url}: the answer is not JSON: {e}")
            ) from e
        return _settle(url, msg)
    if ctype == "text/event-stream":
        data: list[str] = []
        async for line in resp.aiter_lines():
            if line.startswith(":"):
                continue  # an SSE comment (keep-alive)
            if line.startswith("data:"):
                data.append(line[5:].removeprefix(" "))
                continue
            if line == "" and data:
                event, data = "\n".join(data), []
                try:
                    msg = json.loads(event)
                except ValueError as e:
                    raise McpError(
                        refusal(
                            TRANSPORT_ERROR, f"mcp {url}: an SSE event is not JSON: {e}"
                        )
                    ) from e
                if isinstance(msg, dict) and "id" not in msg and "method" in msg:
                    if on_note is not None:
                        await on_note(msg)
                    continue
                if isinstance(msg, dict) and msg.get("id") == rid:
                    return _settle(url, msg)
        raise McpError(
            refusal(
                TRANSPORT_CLOSED,
                f"mcp {url}: the stream ended before the response",
            )
        )
    raise McpError(
        refusal(
            TRANSPORT_ERROR,
            f"mcp {url}: unexpected content-type {ctype or 'none'!r}",
        )
    )
