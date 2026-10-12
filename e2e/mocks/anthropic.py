"""Anthropic Messages — `POST /v1/messages` over SSE.

`url` = `…/v1/messages` (an `anthropic_backend` record's `endpoint`). Key:
`x-api-key: <key>` (the backend sends `anthropic-version: 2023-06-01` too).

Wire shapes, from platform.claude.com/docs (build-with-claude/streaming, api/errors,
api/models):

  GET /v1/models/<id>  ModelInfo {"type":"model","id","display_name","created_at",
                       "max_input_tokens","max_tokens","capabilities":null,
                       "lifecycle":"active", …}; an unknown id → 404
  POST /v1/messages    every frame `event: <type>` + `data: {"type": <type>, …}`:
                       message_start → content_block_start → ping →
                       content_block_delta (text_delta ×2) → content_block_stop →
                       message_delta (stop_reason end_turn, stop_sequence null) →
                       message_stop
  errors               {"type":"error","error":{"type","message"},"request_id"};
                       every answer carries a `request-id` header
"""

from __future__ import annotations

import uuid

from . import MockServer, halves, sse

ERROR_TYPES = {400: "invalid_request_error", 401: "authentication_error",
               402: "billing_error", 403: "permission_error", 404: "not_found_error",
               409: "conflict_error", 413: "request_too_large", 429: "rate_limit_error",
               500: "api_error", 504: "timeout_error", 529: "overloaded_error"}


class AnthropicMock(MockServer):
    base_path = "/v1/messages"
    turn_path = "/v1/messages"

    def __init__(self, *a, **kw) -> None:
        super().__init__(*a, **kw)
        self.request_id = "req_mock" + uuid.uuid4().hex[:16]

    def headers(self):
        return {"request-id": self.request_id}

    def key_of(self, headers):
        return headers.get("x-api-key") or None

    def error_body(self, status, message):
        # 400 also stands in for any other 4xx the docs do not list
        kind = ERROR_TYPES.get(status) or ("invalid_request_error" if status < 500 else "api_error")
        return {"type": "error", "error": {"type": kind, "message": message},
                "request_id": self.request_id}

    def probe(self, method, path):
        if path == f"/v1/models/{self.model}":
            return 200, {"type": "model", "id": self.model, "display_name": self.model,
                         "created_at": "2026-01-01T00:00:00Z", "capabilities": None,
                         "deprecated_at": None, "lifecycle": "active", "line": None,
                         "max_input_tokens": self.context_window,
                         "max_tokens": self.max_tokens, "retires_at": None}
        if path.startswith("/v1/models/"):
            return 404, self.error_body(404, f"model: {path.rsplit('/', 1)[-1]}")
        return None

    def turn(self, h, req, text):
        def ev(kind, **body):
            return sse(kind, {"type": kind, **body})

        p, c = self.usage(text)
        out = [ev("message_start", message={
                   "id": "msg_" + uuid.uuid4().hex[:24], "type": "message",
                   "role": "assistant", "content": [], "model": self.model,
                   "stop_reason": None, "stop_sequence": None,
                   "usage": {"input_tokens": p, "output_tokens": 1}}),
               ev("content_block_start", index=0, content_block={"type": "text", "text": ""}),
               ev("ping")]
        out += [ev("content_block_delta", index=0, delta={"type": "text_delta", "text": p})
                for p in halves(text)]
        out += [ev("content_block_stop", index=0),
                ev("message_delta", delta={"stop_reason": "end_turn", "stop_sequence": None},
                   usage={"output_tokens": c}),
                ev("message_stop")]
        h.stream("text/event-stream; charset=utf-8", out)
