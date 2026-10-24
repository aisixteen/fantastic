"""connectors/mcp_client — a remote MCP server as an agent, end to end. NO MODEL.

A scripted fake MCP server (protocol 2026-07-28, Streamable HTTP — it enforces what
the spec makes a server enforce: headers mirror the body, `_meta` on every request,
the Bearer key) and the kernel's own `mcp_client` agent, on the PACKAGED host of
`RUNTIME` (python | rust), every call through `web_rest`:

  1. boot: `server/discover`, then `tools/list` page by page (by cursor) → the
     agent's verbs; tools it cannot serve are skipped and named;
  2. `docs of=mcp` is the agent's OWN table, built from each tool's schema;
  3. a tool call: arguments → `tools/call`, `Mcp-Name`, `Mcp-Param-Region`;
  4. every result mapping: text, `isError` → error, `structuredContent` →
     `structured`, image base64 → raw bytes (one body per reply, a later one
     `omitted`), an SSE answer (with notifications) settles, `input_required`,
     an unknown `resultType`, an SSE stream with no response, the server's
     -32602 → bad_arg, an unknown tool → unhandled;
  5. THE KEY: an env-backed credential ref (the key in a variable the STORE
     reads, never on a record) → `Authorization: Bearer <key>` on EVERY request;
     the key never in the ledger; a record without `credentials` sends none and
     the server's 401 is `unauthorized`;
  6. A LEGACY SERVER (2025-11-25 Streamable HTTP, shaped like mcp.exa.ai): the
     modern discover is refused with HTTP 400 + a non-modern error → the client
     falls back to `initialize` (2025-11-25, its clientInfo), takes the server's
     session id, sends `notifications/initialized`, then lists and calls with
     `MCP-Session-Id` + `MCP-Protocol-Version` and no modern headers; a server
     ping on the call's stream is answered; reflect names 2025-11-25; a reboot
     ends the old session (DELETE); the key on every request there too.
"""

from __future__ import annotations

import base64
import json
import os
import secrets
import shutil
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
KEY = "mcp-e2e-" + secrets.token_hex(12)
KEY_VAR = "E2E_MCP_KEY"
PNG = b"\x89PNG\r\n\x1a\n\x00\x01raw"
PROTOCOL = "2026-07-28"

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
SERVED = [
    "get_weather",
    "fail",
    "picture",
    "stream",
    "structured",
    "ask",
    "weird",
    "cut",
]


# ─── the fake server ──────────────────────────────────────────────────


class Fake:
    """A 2026-07-28 MCP server on 127.0.0.1: what it saw, and how it answers."""

    def __init__(self):
        self.seen: list[tuple[dict, dict]] = []
        self.lock = threading.Lock()
        fake = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):  # quiet
                pass

            def do_GET(self):  # the 2026-07-28 transport has no GET stream
                self.send_response(405)
                self.end_headers()

            def do_POST(self):
                n = int(self.headers.get("content-length") or 0)
                body = json.loads(self.rfile.read(n) or b"null")
                h = {k.lower(): v for k, v in self.headers.items()}
                with fake.lock:
                    fake.seen.append((h, body))
                status, ctype, payload = fake.answer(h, body)
                raw = (
                    payload
                    if isinstance(payload, bytes)
                    else json.dumps(payload).encode()
                )
                self.send_response(status)
                if ctype:
                    self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

        self.srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.port = self.srv.server_address[1]
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}/mcp"

    def calls(self, method: str) -> list[tuple[dict, dict]]:
        with self.lock:
            return [(h, b) for h, b in self.seen if b.get("method") == method]

    @staticmethod
    def ok(body, result):
        return (
            200,
            "application/json",
            {
                "jsonrpc": "2.0",
                "id": body["id"],
                "result": {"resultType": "complete", **result},
            },
        )

    @staticmethod
    def err(status, body, code, msg):
        return (
            status,
            "application/json",
            {
                "jsonrpc": "2.0",
                "id": body.get("id"),
                "error": {"code": code, "message": msg},
            },
        )

    def answer(self, h: dict, body: dict):
        if h.get("authorization") != f"Bearer {KEY}":
            return 401, None, b""
        meta = (body.get("params") or {}).get("_meta") or {}
        if not {
            "io.modelcontextprotocol/protocolVersion",
            "io.modelcontextprotocol/clientCapabilities",
        } <= set(meta):
            return self.err(400, body, -32602, "missing _meta")
        if (
            h.get("mcp-protocol-version")
            != meta["io.modelcontextprotocol/protocolVersion"]
        ):
            return self.err(400, body, -32020, "protocol header mismatch")
        if h.get("mcp-method") != body.get("method"):
            return self.err(400, body, -32020, "Mcp-Method mismatch")
        accept = h.get("accept", "")
        if "application/json" not in accept or "text/event-stream" not in accept:
            return 406, None, b""
        m = body["method"]
        if m == "server/discover":
            return self.ok(
                body,
                {
                    "supportedVersions": [PROTOCOL],
                    "capabilities": {"tools": {"listChanged": True}},
                    "_meta": {
                        "io.modelcontextprotocol/serverInfo": {
                            "name": "e2e-fake",
                            "version": "1",
                        }
                    },
                    "instructions": "Weather and friends.",
                },
            )
        if m == "tools/list":
            start = int(body["params"].get("cursor") or 0)
            res: dict = {"tools": TOOLS[start : start + 3]}
            if start + 3 < len(TOOLS):
                res["nextCursor"] = str(start + 3)
            return self.ok(body, res)
        if m == "tools/call":
            name = body["params"]["name"]
            if h.get("mcp-name") != name:
                return self.err(400, body, -32020, "Mcp-Name mismatch")
            return self.tool(name, body["params"]["arguments"], body, h)
        return self.err(404, body, -32601, f"no method {m}")

    def tool(self, name, args, body, h):
        b64 = base64.b64encode(PNG).decode()
        if name == "get_weather":
            if "city" not in args:
                return self.err(400, body, -32602, "city required")
            if (
                args.get("region") is not None
                and h.get("mcp-param-region") != args["region"]
            ):
                return self.err(400, body, -32020, "Mcp-Param-Region mismatch")
            return self.ok(
                body,
                {
                    "content": [{"type": "text", "text": f"sunny in {args['city']}"}],
                    "isError": False,
                },
            )
        if name == "fail":
            return self.ok(
                body,
                {
                    "content": [{"type": "text", "text": "no such city"}],
                    "isError": True,
                },
            )
        if name == "picture":
            return self.ok(
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
            return self.ok(
                body,
                {
                    "content": [{"type": "text", "text": '{"t": 21}'}],
                    "structuredContent": {"t": 21},
                },
            )
        if name == "ask":
            return self.ok(
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
            return self.ok(body, {"resultType": "telepathy"})
        events = []
        if name == "stream":
            events = [
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {"progressToken": "p", "progress": 1, "total": 2},
                },
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/message",
                    "params": {"level": "info", "data": "half"},
                },
                {
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "result": {
                        "resultType": "complete",
                        "content": [{"type": "text", "text": "done"}],
                    },
                },
            ]
        if name == "cut":
            events = [
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {"progressToken": "p", "progress": 1},
                }
            ]
        sse = ": keep-alive\n\n" + "".join(f"data: {json.dumps(e)}\n\n" for e in events)
        return 200, "text/event-stream", sse.encode()


# ─── a legacy (initialize-era) server ─────────────────────────────────

LEGACY_REFUSAL = (
    "Bad Request: Unsupported protocol version: 2026-07-28 (supported versions: "
    "2025-11-25, 2025-06-18, 2025-03-26, 2024-11-05, 2024-10-07)"
)


class LegacyFake:
    """A 2025-11-25 Streamable HTTP server on 127.0.0.1 — refuses a modern request
    the way mcp.exa.ai does, negotiates on `initialize`, mints and requires a
    session, requires the version header, puts a ping on the tools/call stream."""

    def __init__(self):
        self.seen: list[tuple[str, dict, dict]] = []
        self.sessions: list[str] = []
        self.deleted: list[str] = []
        self.answers: list[dict] = []
        self.live: str | None = None
        self.lock = threading.Lock()
        fake = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):  # quiet
                pass

            def _send(self, status, head=None, ctype=None, raw=b""):
                self.send_response(status)
                for k, v in (head or {}).items():
                    self.send_header(k, v)
                if ctype:
                    self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_DELETE(self):
                h = {k.lower(): v for k, v in self.headers.items()}
                with fake.lock:
                    if h.get("authorization") == f"Bearer {KEY}":
                        fake.deleted.append(h.get("mcp-session-id"))
                self._send(200)

            def do_GET(self):  # no standalone stream offered
                self._send(405)

            def do_POST(self):
                n = int(self.headers.get("content-length") or 0)
                body = json.loads(self.rfile.read(n) or b"null")
                h = {k.lower(): v for k, v in self.headers.items()}
                with fake.lock:
                    fake.seen.append(("POST", h, body))
                    self._send(*fake.answer(h, body))

        self.srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.port = self.srv.server_address[1]
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}/mcp"

    def methods(self) -> list:
        with self.lock:
            return [b.get("method") for _, _, b in self.seen]

    @staticmethod
    def js(obj, head=None):
        return 200, head, "application/json", json.dumps(obj).encode()

    def answer(self, h: dict, body: dict):
        if h.get("authorization") != f"Bearer {KEY}":
            return 401, None, None, b""
        if "_meta" in (body.get("params") or {}):
            err = {"code": -32000, "message": LEGACY_REFUSAL}
            raw = json.dumps({"jsonrpc": "2.0", "id": None, "error": err}).encode()
            return 400, None, "application/json", raw
        m = body.get("method")
        if m == "initialize":
            self.live = f"sess-{len(self.sessions) + 1}"
            self.sessions.append(self.live)
            res = {
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "e2e-legacy", "version": "1"},
                "instructions": "Old but gold.",
            }
            return self.js(
                {"jsonrpc": "2.0", "id": body["id"], "result": res},
                {"Mcp-Session-Id": self.live},
            )
        if h.get("mcp-session-id") is None:
            return 400, None, "text/plain", b"Bad Request: No valid session ID provided"
        if h.get("mcp-session-id") != self.live:
            return 404, None, "text/plain", b"session not found"
        if h.get("mcp-protocol-version") != "2025-11-25":
            return 400, None, "text/plain", b"bad MCP-Protocol-Version"
        if "mcp-method" in h:
            return 400, None, "text/plain", b"a modern header on a legacy request"
        if m is None:  # the client's answer to our ping
            self.answers.append(body)
            return 202, None, None, b""
        if "id" not in body:  # notifications/initialized
            return 202, None, None, b""
        if m == "tools/list":
            tools = [
                {
                    "name": "echo",
                    "description": "Echoes `say`.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"say": {"type": "string"}},
                        "required": ["say"],
                    },
                }
            ]
            return self.js(
                {"jsonrpc": "2.0", "id": body["id"], "result": {"tools": tools}}
            )
        if m == "tools/call":
            say = body["params"]["arguments"].get("say")
            events = [
                {"jsonrpc": "2.0", "id": "srv-1", "method": "ping"},
                {
                    "jsonrpc": "2.0",
                    "id": body["id"],
                    "result": {"content": [{"type": "text", "text": f"echo {say}"}]},
                },
            ]
            sse = "".join(
                f"id: e{i}\ndata: {json.dumps(e)}\n\n" for i, e in enumerate(events)
            )
            return 200, None, "text/event-stream", sse.encode()
        return self.js(
            {
                "jsonrpc": "2.0",
                "id": body["id"],
                "error": {"code": -32601, "message": f"no {m}"},
            }
        )


# ─── the run ──────────────────────────────────────────────────────────


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    fake = Fake()
    legacy = LegacyFake()
    os.environ[KEY_VAR] = KEY  # the STORE reads it (env backend), never a record
    work = Path(tempfile.mkdtemp(prefix="e2e-mcp-client-"))
    hk = HostKernel(
        spec, work, reach=["mcp", "mcp_keyless", "mcp_legacy", "creds"]
    ).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    try:
        # the ref: the key's variable, pinned to the fake's host:port
        r = hk.send(
            root,
            {
                "type": "update_agent",
                "id": "creds",
                "refs": {
                    "mcp": {
                        "host": f"127.0.0.1:{fake.port}",
                        "env": {"api_key": KEY_VAR},
                    },
                    "legacy": {
                        "host": f"127.0.0.1:{legacy.port}",
                        "env": {"api_key": KEY_VAR},
                    },
                },
                "callable_by": {
                    "$self": "*",
                    "$parent": "*",
                    "mcp": ["get"],
                    "mcp_legacy": ["get"],
                },
            },
        )
        need(
            r.get("updated") is True,
            f"the mcp ref is declared on creds: {str(r)[:200]}",
        )
        rec = hk.create(
            parent_id="rest",
            handler_module="mcp_client.tools",
            id="mcp",
            url=fake.url,
            credentials={"store": "creds", "ref": "mcp"},
            can_call={"$self": "*", "creds": ["get"]},
            callable_by={"$self": "*", "$parent": "*"},
        )
        need(rec.get("id") == "mcp", f"create mcp: {str(rec)[:200]}")

        # 1 · boot = discover + every page of tools/list
        boot = hk.send("mcp", {"type": "boot"})
        need(
            boot.get("tools") == SERVED,
            f"boot lists the servable tools: {str(boot)[:300]}",
        )
        skipped = {s.get("name") for s in boot.get("skipped") or []}
        need(
            skipped == {"reflect", "ps", "fail", "bad_header"},
            f"skipped and named: {skipped}",
        )
        need(
            boot.get("server") == {"name": "e2e-fake", "version": "1"},
            "serverInfo carried",
        )
        cursors = [b["params"].get("cursor") for _, b in fake.calls("tools/list")][-4:]
        need(cursors == [None, "3", "6", "9"], f"tools/list paged by cursor: {cursors}")

        # 2 · docs: the agent's own table
        d = hk.send("kernel", {"type": "docs", "of": "mcp"})
        verbs = d.get("verbs") or {}
        need(
            set(verbs) == {"reflect", "boot", *SERVED}, f"docs of=mcp: {sorted(verbs)}"
        )
        need(
            verbs.get("get_weather") == "args: city:str (req), days:int?, region:str?",
            f"a tool's signature from its schema: {verbs.get('get_weather')!r}",
        )
        rf = hk.send("mcp", {"type": "reflect"})
        need(
            rf.get("tools") == 8
            and rf.get("protocol_version") == PROTOCOL
            and "verbs" not in rf,
            f"reflect: {str(rf)[:200]}",
        )

        # 3 · a call: arguments, Mcp-Name, Mcp-Param-Region
        w = hk.send("mcp", {"type": "get_weather", "city": "Kyiv", "region": "eu-1"})
        need(
            w == {"content": [{"type": "text", "text": "sunny in Kyiv"}]},
            f"get_weather: {w}",
        )
        h, b = (fake.calls("tools/call") or [({}, {"params": {}})])[-1]
        need(
            b["params"].get("arguments") == {"city": "Kyiv", "region": "eu-1"},
            "arguments → tools/call",
        )
        need(
            h.get("mcp-name") == "get_weather" and h.get("mcp-param-region") == "eu-1",
            f"Mcp-Name / Mcp-Param-Region: {h.get('mcp-name')!r} {h.get('mcp-param-region')!r}",
        )

        # 4 · every result mapping
        f = hk.send("mcp", {"type": "fail"})
        need(f.get("error") == "no such city", f"isError → error: {f}")
        s = hk.send("mcp", {"type": "structured"})
        need(s.get("structured") == {"t": 21}, f"structuredContent → structured: {s}")
        p = hk.send("mcp", {"type": "picture"})
        print(
            f"  picture reply keys: {sorted(p)} → {str({k: (v if not isinstance(v, bytes) else f'<{len(v)} bytes>') for k, v in p.items()})[:300]}",
            flush=True,
        )
        content = p.get("content") or []
        need(
            p.get("bytes") == PNG,
            "the first image arrives as raw bytes, the reply's `bytes`",
        )
        first = content[0] if content else {}
        need(
            first.get("body") == "bytes" and first.get("size") == len(PNG),
            f"the image names its body: {first}",
        )
        later = (content[1].get("resource") or {}) if len(content) > 1 else {}
        need(
            later.get("size") == len(PNG) and "omitted" in later,
            f"a later binary item is omitted: {later}",
        )
        st = hk.send("mcp", {"type": "stream"})
        need(
            st == {"content": [{"type": "text", "text": "done"}]},
            f"an SSE answer settles: {st}",
        )
        a = hk.send("mcp", {"type": "ask"})
        need(
            (a.get("input_required") or {}).get("requestState") == "st-1",
            f"input_required: {a}",
        )
        for verb, reason in (
            ("weird", "transport_error"),
            ("cut", "transport_closed"),
            ("get_weather", "bad_arg"),
            ("no_such_tool", "unhandled"),
        ):
            out = hk.send("mcp", {"type": verb})
            need(out.get("reason") == reason, f"{verb} → {reason}: {str(out)[:200]}")

        # 5 · the key
        with fake.lock:
            mine = list(fake.seen)
        need(
            all(h.get("authorization") == f"Bearer {KEY}" for h, _ in mine),
            f"Bearer key on every one of {len(mine)} requests",
        )
        need(
            all(h.get("mcp-protocol-version") == PROTOCOL for h, _ in mine),
            "MCP-Protocol-Version on every request",
        )
        ledger = hk.send("kernel", {"type": "ledger"})
        need(
            KEY not in json.dumps(ledger, default=str),
            "the key never lands in the ledger",
        )
        before = len(fake.seen)
        hk.create(
            parent_id="rest",
            handler_module="mcp_client.tools",
            id="mcp_keyless",
            url=fake.url,
            callable_by={"$self": "*", "$parent": "*"},
        )
        kl = hk.send("mcp_keyless", {"type": "boot"})
        need(
            kl.get("reason") == "unauthorized" and kl.get("http_status") == 401,
            f"keyless → 401 unauthorized: {kl}",
        )
        with fake.lock:
            keyless = fake.seen[before:]
        need(
            bool(keyless) and all("authorization" not in h for h, _ in keyless),
            f"a record without credentials sends no key ({len(keyless)} requests)",
        )

        # 6 · a legacy server: initialize, its session, its version
        rec = hk.create(
            parent_id="rest",
            handler_module="mcp_client.tools",
            id="mcp_legacy",
            url=legacy.url,
            credentials={"store": "creds", "ref": "legacy"},
            can_call={"$self": "*", "creds": ["get"]},
            callable_by={"$self": "*", "$parent": "*"},
        )
        need(rec.get("id") == "mcp_legacy", f"create mcp_legacy: {str(rec)[:200]}")
        lb = hk.send("mcp_legacy", {"type": "boot"})
        need(
            lb.get("protocol_version") == "2025-11-25"
            and lb.get("tools") == ["echo"]
            and lb.get("server") == {"name": "e2e-legacy", "version": "1"},
            f"legacy boot: {str(lb)[:300]}",
        )
        with legacy.lock:
            first = [b.get("method") for _, _, b in legacy.seen[:4]]
            init = next(
                (b for _, _, b in legacy.seen if b.get("method") == "initialize"), {}
            )
        need(
            first
            == [
                "server/discover",
                "initialize",
                "notifications/initialized",
                "tools/list",
            ],
            f"modern probe refused → initialize → initialized → tools/list: {first}",
        )
        ip = init.get("params") or {}
        need(
            ip.get("protocolVersion") == "2025-11-25"
            and ip.get("capabilities") == {}
            and (ip.get("clientInfo") or {}).get("name") == "aisixteen-fantastic",
            f"initialize asks 2025-11-25 with clientInfo: {ip}",
        )
        e = hk.send("mcp_legacy", {"type": "echo", "say": "hi"})
        need(
            e == {"content": [{"type": "text", "text": "echo hi"}]},
            f"a legacy tool call (its stream carried a ping): {e}",
        )
        with legacy.lock:
            _, ch, cb = (
                legacy.seen[-2]
                if legacy.seen[-1][2].get("method") is None
                else legacy.seen[-1]
            )
            answers = list(legacy.answers)
        # create boots once; the explicit boot opened another session
        live = legacy.sessions[-1]
        need(
            ch.get("mcp-session-id") == live
            and ch.get("mcp-protocol-version") == "2025-11-25"
            and "mcp-method" not in ch
            and "_meta" not in (cb.get("params") or {}),
            f"the call carries the session + version, no modern headers: "
            f"{ {k: ch.get(k) for k in ('mcp-session-id', 'mcp-protocol-version', 'mcp-method')} }",
        )
        need(
            answers == [{"jsonrpc": "2.0", "id": "srv-1", "result": {}}],
            f"the server's ping is answered: {answers}",
        )
        lr = hk.send("mcp_legacy", {"type": "reflect"})
        need(
            lr.get("protocol_version") == "2025-11-25"
            and lr.get("instructions") == "Old but gold.",
            f"reflect names the version agreed: {str(lr)[:200]}",
        )
        hk.send("mcp_legacy", {"type": "boot"})
        need(
            legacy.deleted[-1:] == [live]
            and legacy.sessions[-1] != live
            and len(legacy.deleted) == len(legacy.sessions) - 1,
            f"every reboot ends the session it replaces: deleted={legacy.deleted} "
            f"sessions={legacy.sessions}",
        )
        with legacy.lock:
            heads = [h for _, h, _ in legacy.seen]
        need(
            all(h.get("authorization") == f"Bearer {KEY}" for h in heads),
            f"Bearer key on every one of {len(heads)} legacy requests",
        )
        md = hk.send("mcp", {"type": "reflect"})
        need(
            md.get("protocol_version") == PROTOCOL and not fake.calls("initialize"),
            f"the modern agent stays modern: {md.get('protocol_version')}",
        )
    finally:
        hk.close()
        legacy.srv.shutdown()
        fake.srv.shutdown()
        shutil.rmtree(work, ignore_errors=True)

    verdict(
        not bad,
        f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""),
    )
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
