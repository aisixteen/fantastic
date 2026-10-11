"""vLLM (OpenAI-compatible chat) — `POST /v1/chat/completions` over SSE.

`url` = `…/v1` (a `vllm_backend` record's `endpoint`). Key:
`Authorization: Bearer <key>`.

Wire shapes, from vLLM's own server (`vllm/entrypoints/serve/engine/protocol.py`:
`ModelList`/`ModelCard`, `ErrorResponse`/`ErrorInfo`; `vllm/v1/metrics/loggers.py`:
the `vllm:cache_config_info` info-gauge) and the OpenAI `ChatCompletionChunk`
(openai-python `types/chat/chat_completion_chunk.py`):

  GET /v1/models   {"object":"list","data":[{"id","object":"model","created",
                    "owned_by":"vllm","root","parent":null,"max_model_len",
                    "permission":[]}]}
  GET /metrics     Prometheus text; one `vllm:cache_config_info{…} 1.0` line whose
                   labels carry `kv_cache_max_concurrency`
  POST …/chat/completions  `data: {chat.completion.chunk}` frames — the first
                   delta `{"role":"assistant","content":""}`, the answer, a last
                   chunk with `finish_reason:"stop"` and an empty delta — then
                   `data: [DONE]`
  errors           {"error":{"message","type","param":null,"code":<status>}}
"""

from __future__ import annotations

import time
import uuid

from . import MockServer, halves, sse

ERROR_TYPES = {400: "BadRequestError", 401: "Unauthorized", 404: "NotFoundError",
               500: "InternalServerError", 503: "ServiceUnavailable"}


class VllmMock(MockServer):
    base_path = "/v1"
    turn_path = "/v1/chat/completions"

    def __init__(self, *a, kv_slots: int = 4, block_size: int = 16, **kw) -> None:
        super().__init__(*a, **kw)
        self.kv_slots = kv_slots
        self.block_size = block_size
        self.created = int(time.time())

    def error_body(self, status, message):
        return {"error": {"message": message, "type": ERROR_TYPES.get(status, "Error"),
                          "param": None, "code": status}}

    def probe(self, method, path):
        if path == "/v1/models":
            return 200, {"object": "list", "data": [{
                "id": self.model, "object": "model", "created": self.created,
                "owned_by": "vllm", "root": self.model, "parent": None,
                "max_model_len": self.context_window, "permission": []}]}
        if path == "/metrics":
            tokens = self.kv_slots * self.context_window
            labels = {"block_size": str(self.block_size), "cache_dtype": "auto",
                      "enable_prefix_caching": "True", "engine": "0",
                      "kv_cache_max_concurrency": f"{float(self.kv_slots)}",
                      "kv_cache_size_tokens": str(tokens),
                      "num_gpu_blocks": str(tokens // self.block_size)}
            body = ",".join(f'{k}="{v}"' for k, v in labels.items())
            return 200, ("# HELP vllm:cache_config_info Information of the LLMEngine CacheConfig\n"
                         "# TYPE vllm:cache_config_info gauge\n"
                         f"vllm:cache_config_info{{{body}}} 1.0\n")
        return None

    def turn(self, h, req, text):
        cid = "chatcmpl-" + uuid.uuid4().hex
        created = int(time.time())

        def chunk(delta, finish=None):
            return sse(None, {"id": cid, "object": "chat.completion.chunk",
                              "created": created, "model": self.model,
                              "choices": [{"index": 0, "delta": delta, "logprobs": None,
                                           "finish_reason": finish}]})

        out = [chunk({"role": "assistant", "content": ""})]
        out += [chunk({"content": p}) for p in halves(text)]
        out += [chunk({}, "stop"), b"data: [DONE]\n\n"]
        h.stream("text/event-stream; charset=utf-8", out)
