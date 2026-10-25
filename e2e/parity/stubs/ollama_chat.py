"""The ollama surface — `POST <endpoint>/api/chat`, newline-delimited JSON.

Its whole job in §5.4 is to record WHICH PATH the runtime chose (B4.6): python
goes through the `ollama` client library, rust hand-rolls the HTTP, and
a backend that quietly posts to `/v1/chat/completions` instead would still look
like it worked against a permissive server. It cannot look like it worked here —
an unexpected path is recorded and answered 404.
"""

from __future__ import annotations

import json

from . import JSONHandler, StubBase

DONE = '<call name="done"></call>'


class OllamaStub(StubBase):
    def __init__(self, rec, rel: str, port: int) -> None:
        super().__init__(rec, rel, port)
        self.script: list = []
        self.n = 0

    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def set_script(self, script: list) -> None:
        with self.lock:
            self.script, self.n = list(script), 0

    def pick(self):
        with self.lock:
            step = self.n
            self.n += 1
            plan = self.script[step] if step < len(self.script) else DONE
        return (plan if isinstance(plan, dict) else {"text": plan}), step


class Handler(JSONHandler):
    outer: OllamaStub

    def do_POST(self):  # noqa: N802 - stdlib signature
        raw = self.read_body()
        plan, step = self.outer.pick()
        self.outer.record(self.path, self.headers, raw, {"step": step, "path": self.path})
        if not self.path.rstrip("/").endswith("/api/chat"):
            self.answer(404, "application/json",
                        json.dumps({"error": f"stub: no route {self.path}"}).encode())
            return
        if plan.get("status"):
            body = plan.get("body", "")
            data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
            self.answer(int(plan["status"]), "application/json", data)
            return
        text = plan.get("text", DONE)
        lines = [
            json.dumps({"model": "stub", "message": {"role": "assistant", "content": text},
                        "done": False}),
            json.dumps({"model": "stub", "message": {"role": "assistant", "content": ""},
                        "done": True, "done_reason": "stop"}),
        ]
        self.answer(200, "application/x-ndjson", ("\n".join(lines) + "\n").encode())

    def do_GET(self):  # noqa: N802 - stdlib signature
        # `/api/tags` and friends: recorded, refused. A backend that probes the
        # server before chatting is a difference worth seeing.
        self.outer.record(self.path, self.headers, b"", {"method": "GET"})
        self.answer(404, "application/json",
                    json.dumps({"error": f"stub: no route {self.path}"}).encode())
