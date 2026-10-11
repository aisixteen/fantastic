"""Mock model servers — one per provider, standard library only, NO model.

Reusable by any harness (`e2e/`, the canvas e2e): import the provider's class,
`start()` it, point an AI agent's record at `url`, drive turns, read `requests`.

    from mocks.vllm import VllmMock            # url → http://127.0.0.1:<port>/v1
    from mocks.ollama import OllamaMock        # url → http://127.0.0.1:<port>
    from mocks.responses import ResponsesMock  # url → http://127.0.0.1:<port>/v1/responses
    from mocks.anthropic import AnthropicMock  # url → http://127.0.0.1:<port>/v1/messages

    m = VllmMock(model="mock-model").start()   # a free port; `with VllmMock() as m:` too
    m.reply("hello")                           # the next turn answers this text
    m.error(500)                               # the next turn answers HTTP 500
    m.error(503, times=None)                   # every turn, until `m.error(None)`
    m.require_key("sk-1")                      # a request without it → the provider's 401
    m.requests                                 # [{method, path, headers, body, raw, answered}]
    m.keys()                                   # the key each request carried (None = none)
    m.stop()

Every switch is per mock and takes effect on the next request. A turn with no
queued reply answers `DONE` (a `done` call), so an agent's loop always ends.

What each mock serves, beyond its turn path — the probes each backend makes
when its record leaves a field to the server:

  vllm       GET /v1/models {data:[{id, max_model_len}]}, GET /metrics (kv slots)
  ollama     GET /api/tags {models:[{name}]}
  responses  — (no probe)
  anthropic  GET /v1/models/<model> {id, max_tokens, max_input_tokens}

Any other path is recorded and answered 404.
"""

from __future__ import annotations

import json
import socket
import threading
from collections.abc import Callable
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Self

DONE = '<call name="done"></call>'


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class MockServer:
    """A recording, scriptable HTTP server on 127.0.0.1. Subclasses name the
    provider: `base_path`, `key_of`, `error_body`, `turn`, `probe`."""

    #: the path suffix the CLI record's `endpoint` carries
    base_path = ""
    #: the turn path
    turn_path = ""

    def __init__(self, model: str = "mock-model", context_window: int = 262144,
                 max_tokens: int = 262144, port: int | None = None) -> None:
        self.model = model
        self.context_window = context_window
        self.max_tokens = max_tokens
        self.port = port or free_port()
        self.lock = threading.Lock()
        self.requests: list[dict] = []
        self._replies: list[Any] = []
        self._error: dict | None = None
        self._key: str | None = None
        #: `responder(request) -> str | None` answers a turn before the queue;
        #: None falls through to the queue.
        self.responder: Callable[[dict], str | None] | None = None
        self.srv: ThreadingHTTPServer | None = None

    # ── switches ─────────────────────────────────────────────────────────────
    def reply(self, *texts: str) -> Self:
        """Queue answer texts, one per turn, in order."""
        with self.lock:
            self._replies.extend(texts)
        return self

    def error(self, status: int | None, body: Any = None, times: int | None = 1) -> Self:
        """The next `times` turns answer HTTP `status` (None = every turn until
        cleared). `status=None` clears it. `body` replaces the provider's error body."""
        with self.lock:
            self._error = None if status is None else {"status": status, "body": body, "left": times}
        return self

    def require_key(self, key: str | None) -> Self:
        """Refuse (the provider's 401) every request — turn and probe — that does not
        carry `key`. None accepts any request."""
        with self.lock:
            self._key = key
        return self

    def clear(self) -> Self:
        """Forget the recorded requests and the queued replies."""
        with self.lock:
            self.requests.clear()
            self._replies.clear()
        return self

    # ── reading ──────────────────────────────────────────────────────────────
    @property
    def url(self) -> str:
        """What the AI record names as `endpoint`."""
        return f"http://127.0.0.1:{self.port}{self.base_path}"

    @property
    def host(self) -> str:
        """`127.0.0.1:<port>` — what a credential ref pins its key to."""
        return f"127.0.0.1:{self.port}"

    def keys(self) -> list[str | None]:
        return [r["key"] for r in self.requests]

    def turns(self) -> list[dict]:
        """The turn requests alone (no probes, no 404s)."""
        return [r for r in self.requests if r["kind"] == "turn"]

    # ── lifecycle ────────────────────────────────────────────────────────────
    def start(self) -> Self:
        mock = self

        class Handler(_Handler):
            outer = mock

        self.srv = ThreadingHTTPServer(("127.0.0.1", self.port), Handler)
        self.srv.daemon_threads = True
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        return self

    def stop(self) -> None:
        if self.srv is not None:
            self.srv.shutdown()
            self.srv.server_close()
            self.srv = None

    def __enter__(self) -> Self:
        return self.start() if self.srv is None else self

    def __exit__(self, *_exc) -> None:
        self.stop()

    # ── provider hooks ───────────────────────────────────────────────────────
    def key_of(self, headers: dict) -> str | None:
        """The key a request carried (default: `Authorization: Bearer <key>`)."""
        auth = headers.get("authorization") or ""
        return auth[7:] if auth.lower().startswith("bearer ") else (auth or None)

    def error_body(self, status: int, message: str) -> dict:
        return {"error": {"message": message, "type": "mock_error", "code": status}}

    def headers(self) -> dict:
        """Headers the provider puts on every answer (e.g. Anthropic's request-id)."""
        return {}

    def probe(self, method: str, path: str) -> tuple[int, dict] | None:
        """A GET the backend makes to read the server; None = no such path."""
        return None

    def turn(self, h: _Handler, req: dict, text: str) -> None:
        raise NotImplementedError

    # ── plumbing ─────────────────────────────────────────────────────────────
    def next_text(self, req: dict) -> str:
        if self.responder is not None:
            got = self.responder(req)
            if got is not None:
                return got
        with self.lock:
            return self._replies.pop(0) if self._replies else DONE

    def take_error(self) -> dict | None:
        with self.lock:
            e = self._error
            if e is None:
                return None
            if e["left"] is not None:
                e["left"] -= 1
                if e["left"] <= 0:
                    self._error = None
            return e


class _Handler(BaseHTTPRequestHandler):
    outer: MockServer

    def log_message(self, *_a) -> None:  # quiet
        pass

    def _json(self, status: int, obj: Any, extra: dict | None = None) -> None:
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        for k, v in {**self.outer.headers(), **(extra or {})}.items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def stream(self, ctype: str, chunks: list[bytes]) -> None:
        """A streamed 200: the body ends when the connection closes (HTTP/1.0)."""
        self.send_response(200)
        self.send_header("content-type", ctype)
        self.send_header("cache-control", "no-cache")
        for k, v in self.outer.headers().items():
            self.send_header(k, v)
        self.end_headers()
        for c in chunks:
            self.wfile.write(c)
            self.wfile.flush()

    def _handle(self, method: str) -> None:
        m = self.outer
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            body = json.loads(raw) if raw else None
        except ValueError:
            body = None
        headers = {k.lower(): v for k, v in self.headers.items()}
        path = self.path.split("?", 1)[0]
        key = m.key_of(headers)
        entry = {"method": method, "path": path, "headers": headers, "body": body,
                 "raw": raw, "key": key, "kind": "other", "answered": None}
        with m.lock:
            m.requests.append(entry)
            want = m._key

        def done(status: int, kind: str) -> None:
            entry["answered"], entry["kind"] = status, kind

        if want is not None and key != want:
            why = "no key" if key is None else "invalid key"
            done(401, "refused")
            self._json(401, m.error_body(401, f"mock: {why}"))
            return
        if method == "GET":
            got = m.probe(method, path)
            if got is None:
                done(404, "other")
                self._json(404, {"error": f"mock: no route {path}"})
                return
            status, obj = got
            done(status, "probe")
            if isinstance(obj, (bytes, str)):
                data = obj.encode() if isinstance(obj, str) else obj
                self.send_response(status)
                self.send_header("content-type", "text/plain; version=0.0.4; charset=utf-8")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            else:
                self._json(status, obj)
            return
        if path.rstrip("/") != m.turn_path:
            done(404, "other")
            self._json(404, {"error": f"mock: no route {path}"})
            return
        err = m.take_error()
        if err is not None:
            done(err["status"], "turn")
            body = err["body"] if err["body"] is not None else m.error_body(
                err["status"], f"mock: HTTP {err['status']}")
            self._json(err["status"], body)
            return
        text = m.next_text(body if isinstance(body, dict) else {})
        done(200, "turn")
        entry["reply"] = text
        m.turn(self, body if isinstance(body, dict) else {}, text)

    def do_GET(self) -> None:
        self._handle("GET")

    def do_POST(self) -> None:
        self._handle("POST")


def sse(event: str | None, obj: Any) -> bytes:
    head = f"event: {event}\n" if event else ""
    return f"{head}data: {json.dumps(obj)}\n\n".encode()


def halves(text: str) -> list[str]:
    """The answer as two deltas (a stream, not one blob)."""
    mid = len(text) // 2
    return [p for p in (text[:mid], text[mid:]) if p]
