"""The OpenAI-compatible surface — `POST <endpoint>/chat/completions`, SSE.

Both `nvidia_nim_backend` and `llamacpp_backend` speak it
(`llamacpp/.../provider.py:5-8`, `nvidia/.../provider.py`), so one stub serves
both and the diff can show where the two backends put the SAME request
differently — which model field they send, which header carries the key, whether
`temperature` is present at all.

Its scripts carry the provider-failure shapes §5.4 names: a 429 with
`Retry-After`, a 429 twice (the retry bound), and a stream that emits one line,
STALLS, then finishes — the shape that proves a token event reaches the stream
before the whole reply is released (B2.11).
"""

from __future__ import annotations

import json
import time

from . import JSONHandler, StubBase

DONE = '<call name="done"></call>'


class OpenAIChatStub(StubBase):
    def __init__(self, rec, rel: str, port: int) -> None:
        super().__init__(rec, rel, port)
        self.script: list = []
        self.n = 0

    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}/v1"

    def set_script(self, script: list) -> None:
        with self.lock:
            self.script, self.n = list(script), 0

    def pick(self):
        with self.lock:
            step = self.n
            self.n += 1
            plan = self.script[step] if step < len(self.script) else DONE
        return (plan if isinstance(plan, dict) else {"text": plan}), step


def chunk_line(text: str) -> bytes:
    return ("data: " + json.dumps({
        "id": "stub", "object": "chat.completion.chunk", "model": "stub",
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": None}],
    }) + "\n\n").encode()


class Handler(JSONHandler):
    outer: OpenAIChatStub

    def do_POST(self):  # noqa: N802 - stdlib signature
        raw = self.read_body()
        plan, step = self.outer.pick()
        self.outer.record(self.path, self.headers, raw,
                          {"step": step, **{k: v for k, v in plan.items() if k != "text"}})
        if plan.get("status"):
            body = plan.get("body", "")
            data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
            extra = {}
            if plan.get("retry_after") is not None:
                extra["Retry-After"] = str(plan["retry_after"])
            self.answer(int(plan["status"]), plan.get("content_type", "application/json"),
                        data, extra)
            return
        text = plan.get("text", DONE)
        # CHUNKED, so a mid-stream stall is possible: `content-length` would force
        # the whole body out at once and the token-before-release item (B2.11)
        # could not be measured.
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()

        def write(b: bytes) -> None:
            self.wfile.write(f"{len(b):X}\r\n".encode() + b + b"\r\n")
            self.wfile.flush()

        head, _, tail = text.partition("\n")
        write(chunk_line(head + ("\n" if tail else "")))
        if plan.get("stall"):
            time.sleep(float(plan["stall"]))
        if tail:
            write(chunk_line(tail))
        if not plan.get("cut"):
            write(("data: " + json.dumps({
                "id": "stub", "object": "chat.completion.chunk", "model": "stub",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            }) + "\n\n").encode())
            write(b"data: [DONE]\n\n")
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()
