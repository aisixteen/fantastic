"""A scripted OpenAI RESPONSES endpoint (`POST /v1/responses`, SSE), for the
tui runner. No model, no network: each request is answered by the next entry
of the chain, as one text output streamed in two deltas, then
`response.completed` with a reported `input_tokens`. Every request body is
recorded (`requests`), so a scenario can assert what the backend sent.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DONE = '<call name="done"></call>'


class ResponsesStub:
    def __init__(self, port: int, chain: list[str]) -> None:
        self.port = port
        self.chain = list(chain)
        self.requests: list[dict] = []
        self.lock = threading.Lock()
        self.srv: ThreadingHTTPServer | None = None

    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}/v1/responses"

    def next_text(self) -> str:
        with self.lock:
            return self.chain.pop(0) if self.chain else DONE

    def start(self) -> None:
        stub = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_a) -> None:  # quiet
                pass

            def do_POST(self) -> None:  # noqa: N802
                n = int(self.headers.get("content-length") or 0)
                body = json.loads(self.rfile.read(n) or b"{}")
                with stub.lock:
                    stub.requests.append({"path": self.path, "auth": self.headers.get("authorization"), "body": body})
                if self.path != "/v1/responses":
                    self.send_response(404)
                    self.end_headers()
                    return
                text = stub.next_text()
                self.send_response(200)
                self.send_header("content-type", "text/event-stream")
                self.end_headers()
                half = len(text) // 2

                def ev(kind: str, obj: dict) -> None:
                    obj = {"type": kind, **obj}
                    self.wfile.write(f"event: {kind}\ndata: {json.dumps(obj)}\n\n".encode())
                    self.wfile.flush()

                ev("response.created", {"response": {"status": "in_progress"}})
                ev("response.output_item.added", {"item": {"type": "message"}})
                for part in (text[:half], text[half:]):
                    if part:
                        ev("response.output_text.delta", {"delta": part})
                ev("response.completed", {"response": {"status": "completed", "usage": {"input_tokens": 321, "output_tokens": 7},
                                                       "output": [{"type": "message", "content": [{"type": "output_text", "text": text}]}]}})

        self.srv = ThreadingHTTPServer(("127.0.0.1", self.port), Handler)
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def stop(self) -> None:
        if self.srv is not None:
            self.srv.shutdown()
            self.srv.server_close()
