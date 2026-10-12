"""Ollama native — `POST /api/chat`, newline-delimited JSON.

`url` = the bare host (an `ollama_backend` record's `endpoint`). Key:
`Authorization: Bearer <key>` (a door in front of ollama; ollama itself takes none).

Wire shapes, from ollama's own server (`api/types.go`: `ChatResponse`, `Metrics`,
`ListModelResponse`; `server/routes.go`: errors are `gin.H{"error": msg}`) and
`docs/api.md`:

  GET /api/tags   {"models":[{"name","model","modified_at","size","digest",
                   "details":{"parent_model","format","family","families",
                   "parameter_size","quantization_level"}}]}
  POST /api/chat  one JSON object per line: `{"model","created_at","message":
                  {"role":"assistant","content"},"done":false}` per piece, then
                  `{…,"message":{"role":"assistant","content":""},"done":true,
                  "done_reason":"stop", total_duration, load_duration,
                  prompt_eval_count, prompt_eval_duration, eval_count,
                  eval_duration}`
  errors          {"error":"<message>"}
"""

from __future__ import annotations

import json
from datetime import UTC, datetime

from . import MockServer, halves


def _now() -> str:
    return datetime.now(UTC).isoformat().replace("+00:00", "Z")


class OllamaMock(MockServer):
    base_path = ""
    turn_path = "/api/chat"

    def error_body(self, status, message):
        return {"error": message}

    def probe(self, method, path):
        if path == "/api/tags":
            return 200, {"models": [{
                "name": self.model, "model": self.model, "modified_at": _now(),
                "size": 1, "digest": "0" * 64,
                "details": {"parent_model": "", "format": "gguf", "family": "mock",
                            "families": ["mock"], "parameter_size": "1B",
                            "quantization_level": "Q4_K_M"}}]}
        return None

    def turn(self, h, req, text):
        def line(obj):
            return (json.dumps(obj) + "\n").encode()

        out = [line({"model": self.model, "created_at": _now(),
                     "message": {"role": "assistant", "content": p}, "done": False})
               for p in halves(text)]
        p, c = self.usage(text)
        out.append(line({"model": self.model, "created_at": _now(),
                         "message": {"role": "assistant", "content": ""},
                         "done": True, "done_reason": "stop",
                         "total_duration": 1000000, "load_duration": 1000,
                         "prompt_eval_count": p, "prompt_eval_duration": 100000,
                         "eval_count": c, "eval_duration": 100000}))
        h.stream("application/x-ndjson", out)
