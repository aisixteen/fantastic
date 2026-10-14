"""mcp_client — ONE remote MCP server as an agent.

`create_agent handler_module=mcp_client.tools url=<the server's MCP endpoint>
credentials={store, ref}` (optional — a keyless server takes none). Its `boot` asks
the server who it is (`server/discover`) and lists its tools (`tools/list`, every
page); from then on EACH TOOL IS A VERB of this agent: `send {type: <tool>, …its
arguments}` is a `tools/call`. `docs of=<this agent>` lists them — the agent's own
table, built from each tool's `inputSchema` and `description` (per-instance verbs).
A grant names tools like verbs: `can_call {gh: ["create_issue"]}`.

Streamable HTTP, DUAL-ERA (`wire.py`): protocol 2026-07-28 (stateless) first; a
server that speaks only an `initialize`-era version (2025-11-25, 2025-06-18,
2025-03-26, 2024-11-05) is spoken to in that version, through its session.
`reflect` names the version agreed. AUTH = a static key: the ref's
`api_key`, read from the credential store before EVERY request (as an AI backend
reads its key) and sent as `Authorization: Bearer` to the ref's host only.

THE ANSWER: `{content, structured?}` — the tool's content items as the server sent
them, except `image`/`audio` `data` and an embedded resource's `blob`, which arrive as
RAW BYTES (never base64 on our wire) — ONE binary body per reply, at its top-level
`bytes` (the frame codec and every leg carry one, there): the first binary item says
`body: "bytes"`, a later one has `size` and `omitted` instead; `structuredContent`
as `structured`.
A tool error (`isError`) adds `error` (its text), so the call settles `errored`. A
tool that needs input mid-call (`input_required`) answers `{error, input_required}` —
this client cannot give input yet.
"""

from __future__ import annotations

import base64
import binascii
from typing import Any
from urllib.parse import urlsplit

from ai_core.credentials import CredentialError, key_reader
from ai_core.schema import credentials_field, field, validate
from kernel._agent import _SYSTEM_VERBS
from kernel._refusal import (
    NOT_CONNECTED,
    TRANSPORT_ERROR,
    UNAUTHORIZED,
    UNHANDLED,
    refusal,
)

from mcp_client import wire
from mcp_client.wire import McpError, Session

NAME = "mcp_client"
#: Verbs the kernel and this bundle answer first — a tool by one of these names is
#: skipped (listed in reflect `skipped`), never shadowed silently.
RESERVED = frozenset(_SYSTEM_VERBS | {"reflect", "boot", "shutdown"})

RECORD_SCHEMA = {
    "fields": [
        field(
            "url",
            "str",
            "the MCP server's endpoint, e.g. https://mcp.example.com/mcp",
            required=True,
        ),
        credentials_field(required=False),
    ]
}

# instance id → what its server said: {tools, docs, mirrors, server, instructions,
# skipped, error}
_STATE: dict[str, dict] = {}
# instance id → what it agreed with its server (era, version, session id)
_SESSIONS: dict[str, Session] = {}


# ─── the bundle's own verbs ─────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """No args. → {id, sentence, url, protocol_version (the one agreed at boot; null before), server?, instructions?, tools, skipped?, last_error?}."""
    rec = kernel.get(id) or {}
    st = _STATE.get(id) or {}
    sess = _SESSIONS.get(id)
    out: dict[str, Any] = {
        "id": id,
        "sentence": f"The MCP server at {rec.get('url')} — its tools are this "
        f"agent's verbs (docs of={id}).",
        "url": rec.get("url"),
        "protocol_version": sess.version if sess is not None else None,
        "tools": len(st.get("tools") or {}),
    }
    for key in ("server", "instructions"):
        if st.get(key):
            out[key] = st[key]
    if st.get("skipped"):
        out["skipped"] = st["skipped"]
    if st.get("error"):
        out["last_error"] = st["error"]
    return out


async def _boot(id, payload, kernel):
    """No args. Ask the server who it is and list its tools — this agent's verbs. Returns {server, protocol_version, tools:[names], skipped?}.

    Sent again it RE-LISTS (a server's tools changed). A failure keeps the tools
    already listed and answers why; `reflect` shows it as `last_error`."""
    url = (kernel.get(id) or {}).get("url")
    st = _STATE.setdefault(id, {})
    sess = _SESSIONS.setdefault(id, Session())
    try:
        key = await _key(id, kernel, url)
        disc = await wire.connect(url, key, sess)
        caps = disc.get("capabilities") or {}
        tools: list = []
        if "tools" in caps:
            cursor, seen = None, set()
            while True:
                page = await wire.rpc(
                    url,
                    "tools/list",
                    {} if cursor is None else {"cursor": cursor},
                    key=key,
                    sess=sess,
                )
                tools += page.get("tools") or []
                cursor = page.get("nextCursor")
                if not cursor:
                    break
                if cursor in seen:
                    raise McpError(
                        refusal(
                            TRANSPORT_ERROR,
                            f"mcp {url}: tools/list repeated cursor {cursor!r}",
                        )
                    )
                seen.add(cursor)
    except McpError as e:
        st["error"] = e.reply.get("error")
        return e.reply
    table, docs, mir, skipped = {}, {}, {}, []
    for t in tools:
        name = t.get("name") if isinstance(t, dict) else None
        if not isinstance(name, str) or not name:
            skipped.append({"name": name, "why": "a tool with no name"})
            continue
        if name in RESERVED:
            skipped.append(
                {"name": name, "why": "the kernel or this bundle answers that verb"}
            )
            continue
        if name in table:
            skipped.append({"name": name, "why": "listed twice"})
            continue
        # `x-mcp-header` is a 2026-07-28 rule: a legacy server's schema mirrors nothing
        found = wire.mirrors(t.get("inputSchema") or {}) if sess.era == "modern" else []
        if isinstance(found, str):
            skipped.append({"name": name, "why": found})
            continue
        table[name], docs[name], mir[name] = t, tool_doc(t), found
    server = disc.get("server")
    st.clear()
    st.update(
        tools=table,
        docs=docs,
        mirrors=mir,
        server=server,
        instructions=disc.get("instructions"),
        skipped=skipped,
    )
    await kernel.emit(
        id, {"type": "tools_listed", "tools": list(table), "skipped": skipped}
    )
    out: dict[str, Any] = {
        "server": server,
        "protocol_version": sess.version,
        "tools": list(table),
    }
    if skipped:
        out["skipped"] = skipped
    return out


VERBS = {"reflect": _reflect, "boot": _boot}
EMITS = {
    "tools_listed": "{tools:[names], skipped:[{name, why}]} — after every listing (boot)",
    "progress": "{tool, progressToken, progress, total?, message?} — a tool's progress",
    "log": "{tool, level, data, logger?} — a log line a tool sent",
}
NOTES = (
    "A remote MCP server's tools are this agent's verbs (`docs of=<id>`). Record: "
    "`url` (the MCP endpoint), `credentials` ({store, ref} holding `api_key`, sent "
    "as a Bearer key to that host only; absent = keyless). Wiring: this agent's "
    "`can_call` names the store, the store's `callable_by` names this agent. A tool "
    "runs on the remote server with the key's rights — the grant on each tool is "
    "the consent."
)


def instance_verbs(agent) -> dict[str, str]:
    """THIS agent's verbs for `docs` — its own, then the server's tools."""
    own = {n: f.__doc__ or "" for n, f in VERBS.items()}
    return {**own, **((_STATE.get(agent.id) or {}).get("docs") or {})}


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse at the write a record this agent cannot use: a `url` that is not an
    absolute http(s) URL without a fragment, a malformed `credentials`, or a
    missing `url`."""
    url = meta.get("url")
    if url is not None:
        parts = urlsplit(url) if isinstance(url, str) else None
        if (
            parts is None
            or parts.scheme not in ("http", "https")
            or not parts.netloc
            or parts.fragment
        ):
            return (
                f"url={url!r} must be an absolute http(s) URL of the MCP endpoint, "
                "with no fragment (e.g. https://mcp.example.com/mcp)"
            )
    return validate(RECORD_SCHEMA, NAME, meta, record)


async def handler(id: str, payload: dict, kernel) -> dict | None:
    verb = payload.get("type")
    fn = VERBS.get(verb)
    if fn is not None:
        return await fn(id, payload, kernel)
    st = _STATE.get(id) or {}
    tools = st.get("tools")
    if tools is None:
        return refusal(
            NOT_CONNECTED,
            f"{id}: the server's tools are not listed — "
            + (st.get("error") or "boot has not run"),
            hint=f"send boot to {id} once the server is up",
        )
    if verb not in tools:
        return refusal(
            UNHANDLED,
            f"{id}: the MCP server has no tool {verb!r}",
            hint=f"its tools: {', '.join(tools) or 'none'} (docs of={id}); boot "
            "re-lists them",
            verb=verb,
        )
    return await _call(id, verb, payload, kernel, st)


async def on_delete(agent) -> None:
    _STATE.pop(agent.id, None)
    _SESSIONS.pop(agent.id, None)


# ─── a tool call ────────────────────────────────────────────────────


async def _call(id, tool, payload, kernel, st) -> dict:
    url = (kernel.get(id) or {}).get("url")
    arguments = {k: v for k, v in payload.items() if k != "type"}

    async def note(msg: dict) -> None:
        params = msg.get("params") or {}
        if msg.get("method") == "notifications/progress":
            await kernel.emit(id, {"type": "progress", "tool": tool, **params})
        elif msg.get("method") == "notifications/message":
            await kernel.emit(id, {"type": "log", "tool": tool, **params})

    try:
        key = await _key(id, kernel, url)
        headers = wire.param_headers(st["mirrors"][tool], arguments)
        result = await wire.rpc(
            url,
            "tools/call",
            {"name": tool, "arguments": arguments},
            key=key,
            sess=_SESSIONS.setdefault(id, Session()),
            headers=headers,
            on_note=note,
        )
        return answer(url, tool, result)
    except McpError as e:
        return e.reply


def answer(url: str, tool: str, result: dict) -> dict:
    """A `tools/call` result as the caller reads it (the module docstring)."""
    if result.get("resultType") == "input_required":
        return {
            "error": f"{tool}: the server needs input this client cannot give yet",
            "input_required": {
                k: result[k] for k in ("inputRequests", "requestState") if k in result
            },
        }
    box: dict[str, Any] = {"body": None}
    content = [_item(url, c, box) for c in result.get("content") or []]
    out: dict[str, Any] = {"content": content}
    if "structuredContent" in result:
        out["structured"] = result["structuredContent"]
    if result.get("isError") is True:
        text = "\n".join(c["text"] for c in content if c.get("type") == "text")
        out = {"error": text or f"{tool}: the tool reported an error", **out}
    if box["body"] is not None:
        out["bytes"] = box["body"]
    return out


#: What a binary item past the first carries instead of its bytes.
OMITTED = "one binary body per reply — this item's bytes were not sent"


def _item(url: str, c: Any, box: dict) -> Any:
    """One content item: base64 `data` (image, audio) and an embedded resource's
    `blob` decoded to RAW BYTES. A reply carries ONE binary body, at its top-level
    `bytes` (the frame codec and every leg carry one, there): the first binary
    item names it (`size`, `body: "bytes"`), each later one has only its `size`
    and `omitted`."""
    if not isinstance(c, dict):
        return c
    if c.get("type") in ("image", "audio") and isinstance(c.get("data"), str):
        rest = {k: v for k, v in c.items() if k != "data"}
        return {**rest, **_body(url, c["data"], box)}
    res = c.get("resource")
    if c.get("type") == "resource" and isinstance(res, dict) and "blob" in res:
        inner = {k: v for k, v in res.items() if k != "blob"}
        return {**c, "resource": {**inner, **_body(url, res["blob"], box)}}
    return c


def _body(url: str, data: Any, box: dict) -> dict:
    raw = _b64(url, data)
    if box["body"] is not None:
        return {"size": len(raw), "omitted": OMITTED}
    box["body"] = raw
    return {"size": len(raw), "body": "bytes"}


def _b64(url: str, s: Any) -> bytes:
    try:
        return base64.b64decode(s, validate=True)
    except (binascii.Error, TypeError, ValueError) as e:
        raise McpError(
            refusal(
                TRANSPORT_ERROR, f"mcp {url}: invalid base64 in a content item: {e}"
            )
        ) from e


async def _key(id, kernel, url) -> str | None:
    """The ref's `api_key`, read AS this agent, pinned to the server's host — or
    None for a keyless record."""
    try:
        return await key_reader(id, kernel, NAME, required=False)(url)
    except CredentialError as e:
        raise McpError(refusal(UNAUTHORIZED, str(e))) from e


# ─── a tool's doc, from its schema ──────────────────────────────────

_TYPES = {
    "string": "str",
    "integer": "int",
    "number": "float",
    "boolean": "bool",
    "array": "list",
    "object": "dict",
    "null": "null",
}


def _type(s: Any) -> str:
    t = s.get("type") if isinstance(s, dict) else None
    if isinstance(t, list):
        return "|".join(_TYPES.get(x, str(x)) for x in t)
    return _TYPES.get(t, "any") if isinstance(t, str) else "any"


def tool_doc(tool: dict) -> str:
    """`args: a:str (req), b:int?. <description>` — the signature first (what the
    oneliner tier keeps), the tool's description after it."""
    schema = tool.get("inputSchema") or {}
    props = schema.get("properties") if isinstance(schema, dict) else None
    req = set(schema.get("required") or []) if isinstance(schema, dict) else set()
    if isinstance(props, dict) and props:
        args = ", ".join(
            f"{n}:{_type(s)}" + (" (req)" if n in req else "?")
            for n, s in props.items()
        )
        sig = f"args: {args}."
    else:
        sig = "No args."
    desc = (tool.get("description") or tool.get("title") or "").strip()
    return f"{sig} {desc}" if desc else sig
