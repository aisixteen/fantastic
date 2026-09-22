"""Model and peer endpoints the capture owns.

EVERY endpoint a capture reaches is one of these. No family ever names a real
provider, `e2e/backends/connection.local.env` is not read anywhere in
`e2e/parity/`, and an acceptance run therefore spends nothing — which is what
makes it runnable at every landing point in §6 instead of once.

A stub is a SCRIPT plus a RECORDER: it answers by a fixed plan keyed on a marker
the family plants, and it writes down every request it was sent (path, headers,
raw body). Half the items in §5.4 are assertions about the REQUEST — what the
kernel put in the prompt, which endpoint it chose, which header carried the key —
so the request log is evidence, not debugging.
"""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class StubBase:
    """A recording HTTP stub on 127.0.0.1."""

    def __init__(self, rec, rel: str, port: int) -> None:
        self.rec = rec
        self.rel = rel
        self.port = port
        self.lock = threading.Lock()
        self.requests: list[dict] = []
        self.t0 = time.monotonic()
        self.srv: ThreadingHTTPServer | None = None

    # ── lifecycle ────────────────────────────────────────────────────────────
    def start(self, handler_cls) -> str:
        stub = self

        class Handler(handler_cls):
            outer = stub

            def log_message(self, *a):  # noqa: N802 - stdlib signature
                pass

        self.srv = ThreadingHTTPServer(("127.0.0.1", self.port), Handler)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()
        return f"http://127.0.0.1:{self.port}"

    def stop(self) -> None:
        if self.srv is not None:
            self.srv.shutdown()
            self.srv = None

    # ── recording ────────────────────────────────────────────────────────────
    def record(self, path: str, headers, body: bytes, answered: dict) -> int:
        """One request, WHOLE. The body is written raw beside the index: a prompt
        is the measurement, and a prompt summarised is a prompt not measured."""
        with self.lock:
            n = len(self.requests) + 1
            entry = {
                "n": n,
                "path": path,
                "headers": [[k, v] for k, v in headers.items()],
                "body_len": len(body),
                "answered": answered,
            }
            self.requests.append(entry)
        self.rec.raw(f"{self.rel}/req_{n:03d}.bin", body)
        return n

    def flush(self) -> None:
        self.rec.json(f"{self.rel}/requests", self.requests)


def sse(events: list[tuple[str, dict]]) -> bytes:
    return "".join(f"event: {e}\ndata: {json.dumps(d)}\n\n" for e, d in events).encode()


def anthropic_events(text: str, *, stop_reason: str = "end_turn",
                     chunk: int | None = None) -> list[tuple[str, dict]]:
    """The message_start → message_stop shape the anthropic backend parses.

    `chunk` splits the text across several `text_delta`s — the wire family needs
    that (a token event per delta is the thing under test); everything else sends
    one delta so a prompt diff is not drowned in framing.
    """
    parts = [text[i:i + chunk] for i in range(0, len(text), chunk)] if chunk else [text]
    ev = [
        ("message_start", {"type": "message_start", "message": {
            "id": "m", "type": "message", "role": "assistant", "content": [],
            "model": "stub", "stop_reason": None,
            "usage": {"input_tokens": 1, "output_tokens": 0}}}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                 "content_block": {"type": "text", "text": ""}}),
    ]
    ev += [("content_block_delta", {"type": "content_block_delta", "index": 0,
                                    "delta": {"type": "text_delta", "text": c}})
           for c in (parts or [""])]
    ev += [
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                           "delta": {"stop_reason": stop_reason, "stop_sequence": None},
                           "usage": {"output_tokens": 1}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return ev


class JSONHandler(BaseHTTPRequestHandler):
    """Shared plumbing: HTTP/1.1, no logging, a body reader."""

    protocol_version = "HTTP/1.1"

    def read_body(self) -> bytes:
        return self.rfile.read(int(self.headers.get("content-length") or 0))

    def answer(self, status: int, ctype: str, data: bytes, extra: dict | None = None) -> None:
        self.send_response(status)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(data)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(data)
        self.wfile.flush()
