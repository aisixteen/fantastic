"""OpenAI Responses — `POST /v1/responses` over SSE.

`url` = `…/v1/responses` (a `responses_backend` record's `endpoint`). No probe.
Key: `Authorization: Bearer <key>`.

Wire shapes, from openai-python `types/responses/` (`ResponseCreatedEvent`,
`ResponseInProgressEvent`, `ResponseOutputItemAddedEvent`,
`ResponseContentPartAddedEvent`, `ResponseTextDeltaEvent`, `ResponseTextDoneEvent`,
`ResponseContentPartDoneEvent`, `ResponseOutputItemDoneEvent`,
`ResponseCompletedEvent`, `Response`, `ResponseOutputMessage`,
`ResponseOutputText`, `ResponseUsage`). Each frame is `event: <type>` +
`data: {…, "type": <type>, "sequence_number": n}`, in this order:

  response.created → response.in_progress → response.output_item.added →
  response.content_part.added → response.output_text.delta (×2) →
  response.output_text.done → response.content_part.done →
  response.output_item.done → response.completed

  errors  {"error":{"message","type","param":null,"code"}}
"""

from __future__ import annotations

import time
import uuid

from . import MockServer, halves, sse

ERROR_TYPES = {400: "invalid_request_error", 401: "invalid_request_error",
               403: "invalid_request_error", 404: "invalid_request_error",
               429: "rate_limit_exceeded", 500: "server_error", 503: "server_error"}
ERROR_CODES = {401: "invalid_api_key", 429: "rate_limit_exceeded", 500: "server_error"}


class ResponsesMock(MockServer):
    base_path = "/v1/responses"
    turn_path = "/v1/responses"

    def error_body(self, status, message):
        return {"error": {"message": message, "type": ERROR_TYPES.get(status, "server_error"),
                          "param": None, "code": ERROR_CODES.get(status)}}

    def turn(self, h, req, text):
        rid, mid = "resp_" + uuid.uuid4().hex, "msg_" + uuid.uuid4().hex
        created = time.time()
        seq = iter(range(1 << 30))

        def response(status, output, usage=None):
            return {"id": rid, "object": "response", "created_at": created,
                    "status": status, "error": None, "incomplete_details": None,
                    "instructions": req.get("instructions"), "model": self.model,
                    "output": output, "parallel_tool_calls": True, "tool_choice": "auto",
                    "tools": [], "temperature": req.get("temperature"), "top_p": None,
                    "max_output_tokens": req.get("max_output_tokens"),
                    "previous_response_id": None, "usage": usage}

        def ev(kind, **body):
            return sse(kind, {"type": kind, "sequence_number": next(seq), **body})

        part0 = {"type": "output_text", "text": "", "annotations": [], "logprobs": []}
        part = {**part0, "text": text}
        msg0 = {"id": mid, "type": "message", "role": "assistant", "status": "in_progress",
                "content": []}
        msg = {**msg0, "status": "completed", "content": [part]}
        where = {"item_id": mid, "output_index": 0, "content_index": 0}
        p, c = self.usage(text)
        usage = {"input_tokens": p, "input_tokens_details": {"cached_tokens": 0},
                 "output_tokens": c, "output_tokens_details": {"reasoning_tokens": 0},
                 "total_tokens": p + c}
        out = [ev("response.created", response=response("in_progress", [])),
               ev("response.in_progress", response=response("in_progress", [])),
               ev("response.output_item.added", output_index=0, item=msg0),
               ev("response.content_part.added", **where, part=part0)]
        out += [ev("response.output_text.delta", **where, delta=p, logprobs=[])
                for p in halves(text)]
        out += [ev("response.output_text.done", **where, text=text, logprobs=[]),
                ev("response.content_part.done", **where, part=part),
                ev("response.output_item.done", output_index=0, item=msg),
                ev("response.completed", response=response("completed", [msg], usage))]
        h.stream("text/event-stream; charset=utf-8", out)
