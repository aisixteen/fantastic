"""mcp_client — a remote MCP server (protocol 2026-07-28) as an agent whose tools are
its verbs. Against a fake server that enforces the spec's request rules (headers
mirror the body, `_meta` on every request, the Bearer key), through a real
credential store (file backend under tmp_path)."""

from __future__ import annotations

import asyncio
import base64
import json

import httpx
import pytest
from _testkit import as_agent

from mcp_client import wire
from mcp_client.tools import OMITTED

URL = "https://mcp.test/mcp"
KEY = "mcp-key-0123456789"
PNG = b"\x89PNG\r\n\x1a\n\x00\x01raw"

TOOLS = [
    {
        "name": "get_weather",
        "title": "Weather",
        "description": "Current weather for a city. Units default to metric.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "city": {"type": "string"},
                "days": {"type": "integer"},
                "region": {"type": "string", "x-mcp-header": "Region"},
            },
            "required": ["city"],
        },
    },
    {
        "name": "fail",
        "description": "Always a tool error.",
        "inputSchema": {"type": "object"},
    },
    {
        "name": "picture",
        "inputSchema": {"type": "object", "additionalProperties": False},
    },
    {
        "name": "stream",
        "description": "Answers over SSE with progress.",
        "inputSchema": {"type": "object"},
    },
    {"name": "structured", "inputSchema": {"type": "object"}},
    {"name": "ask", "inputSchema": {"type": "object"}},
    {"name": "weird", "inputSchema": {"type": "object"}},
    {"name": "cut", "inputSchema": {"type": "object"}},
    # skipped: a name the kernel answers, a duplicate, an illegal x-mcp-header
    {"name": "reflect", "inputSchema": {"type": "object"}},
    {"name": "ps", "inputSchema": {"type": "object"}},
    {"name": "fail", "inputSchema": {"type": "object"}},
    {
        "name": "bad_header",
        "inputSchema": {
            "type": "object",
            "properties": {
                "xs": {
                    "type": "array",
                    "items": {"type": "string", "x-mcp-header": "X"},
                }
            },
        },
    },
]


class FakeMcp:
    """A 2026-07-28 server: validates what the spec makes it validate, records every
    request, answers by tool."""

    def __init__(self, *, key: str | None = KEY, versions=("2026-07-28",), page=3):
        self.key, self.versions, self.page = key, list(versions), page
        self.seen: list[tuple[httpx.Request, dict]] = []

    async def __call__(self, request: httpx.Request) -> httpx.Response:
        body = json.loads(request.content)
        self.seen.append((request, body))
        h = request.headers
        if self.key is not None and h.get("authorization") != f"Bearer {self.key}":
            return httpx.Response(401, headers={"www-authenticate": "Bearer"})
        meta = body["params"].get("_meta") or {}
        if not {
            "io.modelcontextprotocol/protocolVersion",
            "io.modelcontextprotocol/clientCapabilities",
        } <= set(meta):
            return self._err(400, body, -32602, "missing _meta")
        if (
            h.get("mcp-protocol-version")
            != meta["io.modelcontextprotocol/protocolVersion"]
        ):
            return self._err(400, body, -32020, "protocol header mismatch")
        if h.get("mcp-method") != body["method"]:
            return self._err(400, body, -32020, "Mcp-Method mismatch")
        if "application/json" not in h.get(
            "accept", ""
        ) or "text/event-stream" not in h.get("accept", ""):
            return httpx.Response(406)
        m = body["method"]
        if m == "server/discover":
            return self._ok(
                body,
                {
                    "supportedVersions": self.versions,
                    "capabilities": {"tools": {"listChanged": True}},
                    "_meta": {
                        "io.modelcontextprotocol/serverInfo": {
                            "name": "fake",
                            "version": "9",
                        }
                    },
                    "instructions": "Weather and friends.",
                },
            )
        if m == "tools/list":
            start = int(body["params"].get("cursor") or 0)
            chunk = TOOLS[start : start + self.page]
            res: dict = {"tools": chunk}
            if start + self.page < len(TOOLS):
                res["nextCursor"] = str(start + self.page)
            return self._ok(body, res)
        if m == "tools/call":
            name = body["params"]["name"]
            if h.get("mcp-name") != name:
                return self._err(400, body, -32020, "Mcp-Name mismatch")
            args = body["params"]["arguments"]
            return self.tool(name, args, body, h)
        return self._err(404, body, -32601, f"no method {m}")

    def tool(self, name, args, body, h) -> httpx.Response:
        if name == "get_weather":
            if "city" not in args:
                return self._err(400, body, -32602, "city required")
            if (
                args.get("region") is not None
                and h.get("mcp-param-region") != args["region"]
            ):
                return self._err(400, body, -32020, "Mcp-Param-Region mismatch")
            return self._ok(
                body,
                {
                    "content": [{"type": "text", "text": f"sunny in {args['city']}"}],
                    "isError": False,
                },
            )
        if name == "fail":
            return self._ok(
                body,
                {
                    "content": [{"type": "text", "text": "no such city"}],
                    "isError": True,
                },
            )
        if name == "picture":
            b64 = base64.b64encode(PNG).decode()
            return self._ok(
                body,
                {
                    "content": [
                        {"type": "image", "data": b64, "mimeType": "image/png"},
                        {
                            "type": "resource",
                            "resource": {
                                "uri": "mem://x",
                                "mimeType": "application/octet-stream",
                                "blob": b64,
                            },
                        },
                    ]
                },
            )
        if name == "structured":
            return self._ok(
                body,
                {
                    "content": [{"type": "text", "text": '{"t": 21}'}],
                    "structuredContent": {"t": 21},
                },
            )
        if name == "ask":
            return self._ok(
                body,
                {
                    "resultType": "input_required",
                    "inputRequests": {
                        "who": {
                            "method": "elicitation/create",
                            "params": {"message": "name?"},
                        }
                    },
                    "requestState": "st-1",
                },
            )
        if name == "weird":
            return self._ok(body, {"resultType": "telepathy"})
        events = []
        if name == "stream":
            events.append(
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {"progressToken": "p", "progress": 1, "total": 2},
                }
            )
            events.append(
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/message",
                    "params": {"level": "info", "data": "half"},
                }
            )
            events.append(
                {
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "result": {
                        "resultType": "complete",
                        "content": [{"type": "text", "text": "done"}],
                    },
                }
            )
        if name == "cut":
            events.append(
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {"progressToken": "p", "progress": 1},
                }
            )
        sse = ": keep-alive\n\n" + "".join(f"data: {json.dumps(e)}\n\n" for e in events)
        return httpx.Response(
            200, headers={"content-type": "text/event-stream"}, text=sse
        )

    @staticmethod
    def _ok(body, result) -> httpx.Response:
        result = {"resultType": "complete", **result}
        return httpx.Response(
            200, json={"jsonrpc": "2.0", "id": body["id"], "result": result}
        )

    @staticmethod
    def _err(status, body, code, msg, data=None) -> httpx.Response:
        err = {"code": code, "message": msg}
        if data is not None:
            err["data"] = data
        return httpx.Response(
            status, json={"jsonrpc": "2.0", "id": body["id"], "error": err}
        )

    def calls(self, method: str) -> list[tuple[httpx.Request, dict]]:
        return [(r, b) for r, b in self.seen if b["method"] == method]


@pytest.fixture
def fake(monkeypatch):
    srv = FakeMcp()
    monkeypatch.setattr(wire, "TRANSPORT", httpx.MockTransport(srv))
    return srv


async def _call(k, target, payload, **item):
    return (await k.send([{"target": target, "payload": payload, **item}]))[0]


async def _setup(k, tmp_path, *, host="mcp.test", key=KEY, credentials=True):
    k.ctx.update(k.id, guid="g1")
    k.create(
        "credential_store.tools",
        id="creds",
        file=str(tmp_path / "creds.yaml"),
        refs={"main": {"host": host}},
        callable_by={"mcp": "*"},
    )
    out = await _call(
        k,
        "creds",
        {"type": "set", "ref": "main", "fields": {"api_key": key}},
        secret=["payload.fields"],
    )
    assert out["status"] == "completed", out
    meta = {"url": URL, "can_call": {"creds": "*"}}
    if credentials:
        meta["credentials"] = {"store": "creds", "ref": "main"}
    rec = k.create("mcp_client.tools", id="mcp", **meta)
    assert rec.get("id") == "mcp", rec
    return k


async def _booted(k, tmp_path, **kw):
    await _setup(k, tmp_path, **kw)
    out = await _call(k, "mcp", {"type": "boot"})
    assert out["status"] == "completed", out
    return out["result"]


# ─── listing: discover + every page of tools/list ───────────────────


async def test_boot_lists_every_page_and_skips_what_it_cannot_serve(
    seeded_kernel, tmp_path, fake
):
    r = await _booted(seeded_kernel, tmp_path)
    assert r["server"] == {"name": "fake", "version": "9"}
    assert r["tools"] == [
        "get_weather",
        "fail",
        "picture",
        "stream",
        "structured",
        "ask",
        "weird",
        "cut",
    ]
    skipped = {s["name"]: s["why"] for s in r["skipped"]}
    assert set(skipped) == {"reflect", "ps", "fail", "bad_header"}, skipped
    assert "listed twice" in skipped["fail"]
    assert "properties" in skipped["bad_header"]
    assert len(fake.calls("tools/list")) == 4, "every page, by its cursor"
    assert [b["params"].get("cursor") for _, b in fake.calls("tools/list")] == [
        None,
        "3",
        "6",
        "9",
    ]
    rf = (await _call(seeded_kernel, "mcp", {"type": "reflect"}))["result"]
    assert (
        rf["url"] == URL and rf["protocol_version"] == "2026-07-28" and rf["tools"] == 8
    )
    assert rf["instructions"] == "Weather and friends." and len(rf["skipped"]) == 4
    assert "verbs" not in rf, "reflect says what it is, never how to call it"


async def test_docs_serve_this_agents_own_table_built_from_the_schemas(
    seeded_kernel, tmp_path, fake
):
    k = seeded_kernel
    await _booted(k, tmp_path)
    d = (await _call(k, "kernel", {"type": "docs", "of": "mcp"}))["result"]
    assert d["bundle"] == "mcp_client.tools"
    assert d["verbs"]["get_weather"] == "args: city:str (req), days:int?, region:str?"
    assert d["verbs"]["picture"] == "No args"
    assert set(d["verbs"]) == {
        "reflect",
        "boot",
        "get_weather",
        "fail",
        "picture",
        "stream",
        "structured",
        "ask",
        "weird",
        "cut",
    }
    full = (
        await _call(
            k,
            "kernel",
            {"type": "docs", "of": "mcp", "tier": "detailed", "verb": "get_weather"},
        )
    )["result"]
    assert full["verbs"]["get_weather"].endswith(
        "Current weather for a city. Units default to metric."
    )
    # the BUNDLE (no agent) shows its own verbs only
    b = (await _call(k, "kernel", {"type": "docs", "of": "mcp_client"}))["result"]
    assert set(b["verbs"]) == {"reflect", "boot"}


# ─── the wire: what every request carries, the key included ─────────


async def test_every_request_carries_meta_mirrored_headers_and_the_key(
    seeded_kernel, tmp_path, fake
):
    k = seeded_kernel
    await _booted(k, tmp_path)
    out = await _call(
        k, "mcp", {"type": "get_weather", "city": "Kyiv", "region": "eu-1"}
    )
    assert out["status"] == "completed", out
    assert out["result"] == {"content": [{"type": "text", "text": "sunny in Kyiv"}]}
    for req, body in fake.seen:
        assert req.method == "POST" and str(req.url) == URL
        assert req.headers["authorization"] == f"Bearer {KEY}", (
            "the key on EVERY request"
        )
        assert req.headers["mcp-protocol-version"] == "2026-07-28"
        assert (
            body["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"]
            == "aisixteen-fantastic"
        )
    req, body = fake.calls("tools/call")[-1]
    assert body["params"] == {
        "name": "get_weather",
        "arguments": {"city": "Kyiv", "region": "eu-1"},
        "_meta": body["params"]["_meta"],
    }
    assert req.headers["mcp-name"] == "get_weather"
    assert req.headers["mcp-param-region"] == "eu-1"
    # an argument left out sends no header
    await _call(k, "mcp", {"type": "get_weather", "city": "Lviv"})
    assert "mcp-param-region" not in fake.calls("tools/call")[-1][0].headers
    ledger = await _call(k, "kernel", {"type": "ledger"})
    assert KEY not in repr(ledger), "the key never lands in the ledger"


async def test_a_record_without_credentials_sends_no_key(
    seeded_kernel, tmp_path, monkeypatch
):
    srv = FakeMcp(key=None)
    monkeypatch.setattr(wire, "TRANSPORT", httpx.MockTransport(srv))
    await _booted(seeded_kernel, tmp_path, credentials=False)
    assert all("authorization" not in r.headers for r, _ in srv.seen)


async def test_a_key_pinned_to_another_host_is_never_sent(
    seeded_kernel, tmp_path, fake
):
    k = seeded_kernel
    await _setup(k, tmp_path, host="elsewhere.test")
    out = await _call(k, "mcp", {"type": "boot"})
    assert out["status"] == "errored" and out["result"]["reason"] == "unauthorized", out
    assert fake.seen == [], "nothing left for the server"


async def test_a_refused_key_is_unauthorized(seeded_kernel, tmp_path, fake):
    await _setup(seeded_kernel, tmp_path, key="wrong-key")
    out = await _call(seeded_kernel, "mcp", {"type": "boot"})
    assert (
        out["result"]["reason"] == "unauthorized"
        and out["result"]["http_status"] == 401
    ), out


def test_header_values_ride_plain_or_as_the_base64_sentinel():
    assert wire.header_value("us-west1") == "us-west1"
    assert wire.header_value("Hello, 世界") == "=?base64?SGVsbG8sIOS4lueVjA==?="
    assert wire.header_value(" padded ") == "=?base64?IHBhZGRlZCA=?="
    assert (
        wire.header_value("=?base64?literal?=") == "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
    )
    found = wire.mirrors(TOOLS[0]["inputSchema"])
    assert found == [("Region", ["region"])]
    assert wire.param_headers(found, {"region": "日本"}) == {
        "Mcp-Param-Region": "=?base64?5pel5pys?="
    }
    assert wire.param_headers([("On", ["on"])], {"on": True}) == {
        "Mcp-Param-On": "true"
    }
    assert isinstance(
        wire.mirrors({"properties": {"n": {"type": "number", "x-mcp-header": "N"}}}),
        str,
    )
    assert isinstance(
        wire.mirrors(
            {
                "properties": {
                    "a": {"type": "string", "x-mcp-header": "A"},
                    "b": {"type": "string", "x-mcp-header": "a"},
                }
            }
        ),
        str,
    )


# ─── answers ─────────────────────────────────────────────────────────


async def test_a_tool_error_settles_errored_with_its_text(
    seeded_kernel, tmp_path, fake
):
    await _booted(seeded_kernel, tmp_path)
    out = await _call(seeded_kernel, "mcp", {"type": "fail"})
    assert out["status"] == "errored", out
    assert out["result"]["error"] == "no such city"
    assert out["result"]["content"] == [{"type": "text", "text": "no such city"}]


async def test_binary_content_arrives_as_raw_bytes(seeded_kernel, tmp_path, fake):
    await _booted(seeded_kernel, tmp_path)
    r = (await _call(seeded_kernel, "mcp", {"type": "picture"}))["result"]
    img, res = r["content"]
    # ONE binary body, at the reply's top-level `bytes`; the item names it
    assert r["bytes"] == PNG
    assert img == {
        "type": "image",
        "mimeType": "image/png",
        "size": len(PNG),
        "body": "bytes",
    }
    # ONE binary body per reply (the frame codec): a later one says what it was
    assert res["resource"] == {
        "uri": "mem://x",
        "mimeType": "application/octet-stream",
        "size": len(PNG),
        "omitted": OMITTED,
    }


async def test_structured_content_is_kept(seeded_kernel, tmp_path, fake):
    await _booted(seeded_kernel, tmp_path)
    r = (await _call(seeded_kernel, "mcp", {"type": "structured"}))["result"]
    assert r["structured"] == {"t": 21}


async def test_an_sse_answer_settles_and_its_notes_reach_the_watchers(
    seeded_kernel, tmp_path, fake
):
    k = seeded_kernel
    await _booted(k, tmp_path)
    inbox = k.ctx.ensure_inbox("_t_watch")
    k.watch("mcp", "_t_watch")
    out = await _call(k, "mcp", {"type": "stream"})
    assert out["result"] == {"content": [{"type": "text", "text": "done"}]}, out
    events = []
    while not inbox.empty():
        events.append(inbox.get_nowait())
    kinds = [e.get("type") for e in events]
    assert "progress" in kinds and "log" in kinds, kinds
    prog = next(e for e in events if e.get("type") == "progress")
    assert prog["tool"] == "stream" and prog["progress"] == 1 and prog["total"] == 2


async def test_input_required_answers_what_the_server_asked(
    seeded_kernel, tmp_path, fake
):
    await _booted(seeded_kernel, tmp_path)
    out = await _call(seeded_kernel, "mcp", {"type": "ask"})
    assert out["status"] == "errored"
    assert out["result"]["input_required"]["requestState"] == "st-1"
    assert "who" in out["result"]["input_required"]["inputRequests"]


@pytest.mark.parametrize(
    "payload, reason",
    [
        ({"type": "weird"}, "transport_error"),  # an unknown resultType
        ({"type": "cut"}, "transport_closed"),  # SSE ended with no response
        ({"type": "get_weather"}, "bad_arg"),  # the server's -32602
        ({"type": "no_such_tool"}, "unhandled"),  # not one of its tools
    ],
)
async def test_failures_are_refusals_from_the_closed_set(
    seeded_kernel, tmp_path, fake, payload, reason
):
    await _booted(seeded_kernel, tmp_path)
    out = await _call(seeded_kernel, "mcp", payload)
    assert out["result"].get("reason") == reason, out


async def test_an_unknown_tool_names_the_tools_there_are(seeded_kernel, tmp_path, fake):
    await _booted(seeded_kernel, tmp_path)
    r = (await _call(seeded_kernel, "mcp", {"type": "nope"}))["result"]
    assert "get_weather" in r["hint"] and "docs of=mcp" in r["hint"]


# ─── versions, connection, lifecycle ────────────────────────────────


async def test_a_server_on_an_older_protocol_is_refused_naming_its_versions(
    seeded_kernel, tmp_path, monkeypatch
):
    srv = FakeMcp(versions=["2025-11-25"])
    monkeypatch.setattr(wire, "TRANSPORT", httpx.MockTransport(srv))
    await _setup(seeded_kernel, tmp_path)
    out = await _call(seeded_kernel, "mcp", {"type": "boot"})
    assert out["result"]["reason"] == "transport_error"
    assert (
        "2025-11-25" in out["result"]["error"]
        and "2026-07-28 only" in out["result"]["error"]
    )


async def test_an_unsupported_version_error_names_the_servers_versions(
    seeded_kernel, tmp_path, monkeypatch
):
    async def handler(request):
        body = json.loads(request.content)
        err = {
            "code": -32022,
            "message": "Unsupported protocol version",
            "data": {"supported": ["2025-06-18"], "requested": "2026-07-28"},
        }
        return httpx.Response(
            400, json={"jsonrpc": "2.0", "id": body["id"], "error": err}
        )

    monkeypatch.setattr(wire, "TRANSPORT", httpx.MockTransport(handler))
    await _setup(seeded_kernel, tmp_path)
    r = (await _call(seeded_kernel, "mcp", {"type": "boot"}))["result"]
    assert r["reason"] == "transport_error" and "2025-06-18" in r["error"], r


async def test_an_unreachable_server_is_not_connected_and_calls_say_so(
    seeded_kernel, tmp_path, monkeypatch
):
    def down(request):
        raise httpx.ConnectError("connection refused", request=request)

    monkeypatch.setattr(wire, "TRANSPORT", httpx.MockTransport(down))
    k = seeded_kernel
    await _setup(k, tmp_path)
    boot = await _call(k, "mcp", {"type": "boot"})
    assert boot["result"]["reason"] == "not_connected", boot
    call = (await _call(k, "mcp", {"type": "get_weather", "city": "x"}))["result"]
    assert call["reason"] == "not_connected" and "cannot connect" in call["error"]
    assert (await _call(k, "mcp", {"type": "reflect"}))["result"]["last_error"]


async def test_a_failed_relist_keeps_the_tools_already_listed(
    seeded_kernel, tmp_path, fake, monkeypatch
):
    k = seeded_kernel
    await _booted(k, tmp_path)

    def down(request):
        raise httpx.ConnectError("gone", request=request)

    monkeypatch.setattr(wire, "TRANSPORT", httpx.MockTransport(down))
    assert (await _call(k, "mcp", {"type": "boot"}))["result"][
        "reason"
    ] == "not_connected"
    d = (await _call(k, "kernel", {"type": "docs", "of": "mcp"}))["result"]
    assert "get_weather" in d["verbs"], "the earlier listing stands"


async def test_a_grant_names_tools_like_verbs(seeded_kernel, tmp_path, fake):
    k = seeded_kernel
    await _booted(k, tmp_path)
    k.create(
        "kernel_state.tools",
        id="caller",
        watch=False,
        can_call={"mcp": ["get_weather"]},
    )
    k.update("mcp", callable_by={"caller": ["get_weather", "fail"]})
    with as_agent(k, "caller"):
        ok = await _call(k, "mcp", {"type": "get_weather", "city": "Kyiv"})
        no = await _call(k, "mcp", {"type": "fail"})
    assert ok["status"] == "completed", ok
    assert (
        no["result"]["reason"] == "unauthorized" and no["result"]["side"] == "egress"
    ), no


@pytest.mark.parametrize(
    "meta, fragment",
    [
        ({}, "required on the record and missing: url"),
        ({"url": "mcp.test/mcp"}, "absolute http(s) URL"),
        ({"url": "https://mcp.test/mcp#frag"}, "no fragment"),
        ({"url": URL, "credentials": {"store": "creds"}}, "credentials must be"),
    ],
)
def test_the_record_is_checked_at_the_write(meta, fragment):
    from mcp_client.tools import validate_meta

    err = validate_meta(meta, {})
    assert err is not None and fragment in err, err


async def test_the_catalog_publishes_the_record_schema(seeded_kernel):
    cat = (
        await _call(
            seeded_kernel,
            "kernel",
            {"type": "reflect", "tree": "none", "bundles": "all"},
        )
    )["result"]["bundles"]
    row = next(b for b in cat if b["name"] == "mcp_client")
    names = [f["name"] for f in row["schema"]["fields"]]
    assert names == ["url", "credentials"]


async def test_cancelling_a_call_closes_its_stream(
    seeded_kernel, tmp_path, fake, monkeypatch
):
    k = seeded_kernel
    await _booted(k, tmp_path)
    started = asyncio.Event()

    async def slow(request):
        started.set()
        await asyncio.sleep(30)
        return httpx.Response(200)

    monkeypatch.setattr(wire, "TRANSPORT", httpx.MockTransport(slow))
    out = await _call(k, "mcp", {"type": "get_weather", "city": "x"}, timeout=0.5)
    assert out["status"] == "timed_out", out
    assert started.is_set()
