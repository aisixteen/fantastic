"""The MCP wire — Streamable HTTP, DUAL-ERA (owner, 2026-10-12): the modern protocol
2026-07-28 first, the `initialize`-era ones (2025-11-25, 2025-06-18, 2025-03-26,
2024-11-05) when the server speaks only those — the spec's "dual-era client"
(2026-07-28 basic/versioning + transports/streamable-http, Backward Compatibility).

MODERN (2026-07-28): every JSON-RPC request is its own POST to the server's one
endpoint, carrying its version, identity and capabilities in `params._meta` (no
handshake, no session) and mirrored into headers: `MCP-Protocol-Version`,
`Mcp-Method`, `Mcp-Name` (a `tools/call`'s tool), `Mcp-Param-<name>` (each
`x-mcp-header` argument). The answer is one JSON object or an SSE stream scoped to
the request (its notifications, then the response).

DETECTION (`connect`): `server/discover` is sent modern. A success naming 2026-07-28
stays modern. A recognized modern error (`-32022` UnsupportedProtocolVersion,
`-32021`, `-32020`) is a MODERN server: `-32022` (or a discover) listing only legacy
versions goes legacy, anything else is refused. Any other 4xx (or an error body
that is not a recognized modern one — `-32601` included: every modern server
implements discover) is a LEGACY server → `initialize`.

LEGACY (2025-11-25 Streamable HTTP): `initialize {protocolVersion: 2025-11-25,
capabilities: {}, clientInfo}` → the server's version (one of `LEGACY`, else
refused) and maybe an `MCP-Session-Id`; then `notifications/initialized` (202).
Every later POST carries `MCP-Session-Id` (when one was given) and
`MCP-Protocol-Version` (from 2025-06-18 on — the version that defined it); no
`_meta`, no mirrored headers. A 404 on a session is an expired session: one new
`initialize`, then the request again. A server REQUEST on an SSE stream is answered
(a `ping` with `{}`, anything else `-32601` — this client offers no capability). A
new session ends the old one (HTTP DELETE; a 405 is fine). The deprecated HTTP+SSE
transport (2024-11-05's two endpoints) is not spoken.

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

#: The modern protocol: per-request `_meta`, no handshake.
PROTOCOL = "2026-07-28"
#: The `initialize`-era protocols this client speaks over Streamable HTTP, newest first.
LEGACY = ("2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05")
#: Legacy revisions from this one on define the `MCP-Protocol-Version` header.
VERSION_HEADER_FROM = "2025-06-18"
#: Every protocol this client speaks, as an error names them.
SPOKEN = f"{PROTOCOL}, {', '.join(LEGACY)}"
#: The modern JSON-RPC errors that identify a modern server (never a fallback).
MODERN_ERRORS = (-32022, -32021, -32020)
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


class Session:
    """What one agent agreed with its server: the era, the protocol version, and a
    legacy server's session id (None when it gave none)."""

    def __init__(self) -> None:
        self.era: str | None = None  # "modern" | "legacy"
        self.version: str | None = None
        self.session_id: str | None = None


class _Legacy(Exception):
    """The modern probe was answered the way only a legacy server answers."""


class _Expired(Exception):
    """HTTP 404 on a request carrying a session id — the session is gone."""


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
                f"{SPOKEN}",
                mcp_code=code,
                supported=supported,
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


async def _answer_server_request(url, msg: dict, key, sess: Session) -> None:
    """A legacy server's own REQUEST on an SSE stream: a `ping` is answered `{}`;
    anything else `-32601` — this client declares no capability."""
    if msg.get("method") == "ping":
        reply = {"jsonrpc": "2.0", "id": msg.get("id"), "result": {}}
    else:
        reply = {
            "jsonrpc": "2.0",
            "id": msg.get("id"),
            "error": {
                "code": -32601,
                "message": f"this client does not serve {msg.get('method')!r}",
            },
        }
    await _send_message(url, reply, key, sess)


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


_BASE_HEADERS = {
    "Accept": "application/json, text/event-stream",
    "Content-Type": "application/json",
}


async def connect(url: str, key: str | None, sess: Session) -> dict:
    """Detect the server's era and agree a version (the module docstring) — fills
    `sess`; → `{server, instructions, capabilities}`. A legacy session `sess` held is
    ended first (DELETE)."""
    old = sess.session_id
    sess.era = sess.version = sess.session_id = None
    if old is not None:
        await _end_session(url, key, old)
    try:
        disc = await _modern(url, "server/discover", {}, key, None, None, probe=True)
    except _Legacy:
        return await _initialize(url, key, sess)
    except McpError as e:
        supported = e.reply.get("supported")
        if isinstance(supported, list) and any(v in LEGACY for v in supported):
            return await _initialize(url, key, sess)
        raise
    versions = disc.get("supportedVersions") or []
    if PROTOCOL not in versions:
        if any(v in LEGACY for v in versions):
            return await _initialize(url, key, sess)
        raise McpError(
            refusal(
                TRANSPORT_ERROR,
                f"mcp {url}: the server speaks {versions} — this client speaks {SPOKEN}",
            )
        )
    sess.era, sess.version = "modern", PROTOCOL
    return {
        "server": (disc.get("_meta") or {}).get("io.modelcontextprotocol/serverInfo"),
        "instructions": disc.get("instructions"),
        "capabilities": disc.get("capabilities") or {},
    }


async def rpc(
    url: str,
    method: str,
    params: dict,
    *,
    key: str | None,
    sess: Session,
    headers: dict | None = None,
    on_note: Callable[[dict], Awaitable[None]] | None = None,
) -> dict:
    """POST one request in the agreed era; → its `result`, or raise `McpError`.
    Notifications on an SSE answer go to `on_note` as they arrive. `headers` (the
    mirrored `Mcp-Param-*`) ride a modern request only."""
    if sess.era != "legacy":
        return await _modern(url, method, params, key, headers, on_note)
    try:
        return await _legacy(url, method, params, key, sess, on_note)
    except _Expired:
        await _initialize(url, key, sess)
        return await _legacy(url, method, params, key, sess, on_note)


async def _modern(url, method, params, key, headers, on_note, *, probe=False) -> dict:
    rid = str(next(_ids))
    body = {
        "jsonrpc": "2.0",
        "id": rid,
        "method": method,
        "params": {**params, "_meta": _meta()},
    }
    head = {**_BASE_HEADERS, "MCP-Protocol-Version": PROTOCOL, "Mcp-Method": method}
    if method == "tools/call":
        head["Mcp-Name"] = header_value(params["name"])
    head.update(headers or {})
    result, _ = await _post(url, body, head, key, on_note, rid=rid, probe=probe)
    return result


def _legacy_head(sess: Session) -> dict:
    head = dict(_BASE_HEADERS)
    if sess.version is not None and sess.version >= VERSION_HEADER_FROM:
        head["MCP-Protocol-Version"] = sess.version
    if sess.session_id is not None:
        head["MCP-Session-Id"] = sess.session_id
    return head


async def _legacy(url, method, params, key, sess: Session, on_note) -> dict:
    rid = str(next(_ids))
    body = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
    result, _ = await _post(
        url, body, _legacy_head(sess), key, on_note, rid=rid, sess=sess
    )
    return result


async def _initialize(url: str, key: str | None, sess: Session) -> dict:
    """The legacy handshake: `initialize` at the newest legacy version, the server's
    answer (its version must be one this client speaks), its session id, then
    `notifications/initialized`."""
    sess.era, sess.version, sess.session_id = "legacy", None, None
    rid = str(next(_ids))
    body = {
        "jsonrpc": "2.0",
        "id": rid,
        "method": "initialize",
        "params": {
            "protocolVersion": LEGACY[0],
            "capabilities": {},
            "clientInfo": CLIENT_INFO,
        },
    }
    result, head = await _post(url, body, dict(_BASE_HEADERS), key, None, rid=rid)
    version = result.get("protocolVersion")
    if version not in LEGACY:
        sess.era = None
        raise McpError(
            refusal(
                TRANSPORT_ERROR,
                f"mcp {url}: the server answered initialize with protocol "
                f"{version!r} — this client speaks {SPOKEN}",
            )
        )
    sess.version = version
    sid = head.get("mcp-session-id")
    sess.session_id = sid if sid else None
    await _send_message(
        url,
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        key,
        sess,
    )
    return {
        "server": result.get("serverInfo"),
        "instructions": result.get("instructions"),
        "capabilities": result.get("capabilities") or {},
    }


async def _send_message(url: str, msg: dict, key, sess: Session) -> None:
    """POST a notification or a response (legacy): the server answers 202 Accepted."""
    head = _legacy_head(sess)
    if key is not None:
        head["Authorization"] = f"Bearer {key}"
    try:
        async with httpx.AsyncClient(
            timeout=httpx.Timeout(None, connect=CONNECT_TIMEOUT), transport=TRANSPORT
        ) as client:
            resp = await client.post(url, json=msg, headers=head)
    except (httpx.ConnectError, httpx.ConnectTimeout) as e:
        raise McpError(refusal(NOT_CONNECTED, f"mcp {url}: cannot connect: {e}")) from e
    except httpx.HTTPError as e:
        raise McpError(refusal(TRANSPORT_ERROR, f"mcp {url}: {e}")) from e
    if resp.status_code >= 300:
        what = msg.get("method") or f"the response to {msg.get('id')!r}"
        raise McpError(
            refusal(
                TRANSPORT_ERROR,
                f"mcp {url}: {what} answered HTTP {resp.status_code} (202 expected): "
                f"{resp.text[:200]}",
                http_status=resp.status_code,
            )
        )


async def _end_session(url: str, key, sid: str) -> None:
    """End a legacy session (HTTP DELETE) — best effort: a server may answer 405, or
    be gone."""
    head = {"MCP-Session-Id": sid}
    if key is not None:
        head["Authorization"] = f"Bearer {key}"
    try:
        async with httpx.AsyncClient(
            timeout=httpx.Timeout(None, connect=CONNECT_TIMEOUT), transport=TRANSPORT
        ) as client:
            await client.delete(url, headers=head)
    except httpx.HTTPError:
        pass


async def _post(url, body, head, key, on_note, *, rid, probe=False, sess=None):
    """One request POSTed; → (its result, the response headers)."""
    if key is not None:
        head["Authorization"] = f"Bearer {key}"
    try:
        async with httpx.AsyncClient(
            timeout=httpx.Timeout(None, connect=CONNECT_TIMEOUT), transport=TRANSPORT
        ) as client:
            async with client.stream("POST", url, json=body, headers=head) as resp:
                result = await _read(url, rid, resp, key, on_note, probe, sess)
                return result, resp.headers
    except (httpx.ConnectError, httpx.ConnectTimeout) as e:
        raise McpError(refusal(NOT_CONNECTED, f"mcp {url}: cannot connect: {e}")) from e
    except httpx.HTTPError as e:
        raise McpError(refusal(TRANSPORT_ERROR, f"mcp {url}: {e}")) from e


def _legacy_answer(err: Any) -> bool:
    """A probe's error body is NOT a recognized modern error — a legacy server."""
    return not (isinstance(err, dict) and err.get("code") in MODERN_ERRORS)


async def _read(url, rid, resp: httpx.Response, key, on_note, probe, sess) -> dict:
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
    if status == 404 and sess is not None and sess.session_id is not None:
        raise _Expired()
    if status >= 400:
        raw = await resp.aread()
        try:
            msg = json.loads(raw)
        except ValueError:
            msg = None
        err = msg.get("error") if isinstance(msg, dict) else None
        if probe and status < 500 and _legacy_answer(err):
            raise _Legacy()
        if isinstance(err, dict):
            raise _protocol_error(url, err)
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
        if probe and isinstance(msg, dict) and "error" in msg:
            if _legacy_answer(msg["error"]):
                raise _Legacy()
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
                if isinstance(msg, dict) and "method" in msg and sess is not None:
                    await _answer_server_request(url, msg, key, sess)
                    continue
                if isinstance(msg, dict) and msg.get("id") == rid:
                    if probe and "error" in msg and _legacy_answer(msg["error"]):
                        raise _Legacy()
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
