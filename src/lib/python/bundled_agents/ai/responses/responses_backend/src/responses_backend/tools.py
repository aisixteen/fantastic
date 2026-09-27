"""responses bundle — reflect-driven LLM agent over the OpenAI Responses API.

A THIN binding over `ai_core` (see anthropic_backend.tools for the pattern): this
module supplies the ResponsesProvider builder + the backend's identity, and
`ai_core.build()` returns the `(VERBS, handler)` bound to it. ALL shared machinery
(queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, context management, the
verb bodies) lives in `ai_core.core`. State dicts + the patchable `SEND_TIMEOUT` /
`MAX_CALL_DEPTH` are re-exported from `ai_core.core` so the monkeypatch seams keep
working on the shared path — and so is `on_cancel`, the shared bundle CANCEL hook
the substrate discovers on this module (an in-flight `send` is a kernel TASK,
killed with the universal `ps` → `cancel` system verbs).

Distinct from `nvidia_nim_backend` / `llamacpp_backend` on purpose: those speak
OpenAI Chat Completions (`/chat/completions`), this one the Responses wire
(`/responses`) — a different request shape and a different event stream.
"""

from __future__ import annotations

from ai_core import build
from ai_core.core import (  # noqa: F401 — re-exported test seams + the cancel hook
    DEFAULT_CLIENT_ID,
    MAX_CALL_DEPTH,
    SEND_TIMEOUT,
    _assemble,
    _current,
    _locks,
    _providers,
    _queue,
    on_cancel,
)


def make_provider(id, kernel):
    """Build a ResponsesProvider from the agent record (endpoint/model/max_tokens/sampling)."""
    from responses_backend.provider import (
        DEFAULT_ENDPOINT,
        DEFAULT_MODEL,
        MAX_TOKENS,
        ResponsesProvider,
    )

    rec = kernel.get(id) or {}
    return ResponsesProvider(
        endpoint=rec.get("endpoint", DEFAULT_ENDPOINT),
        model=rec.get("model", DEFAULT_MODEL),
        max_tokens=int(rec.get("max_tokens") or MAX_TOKENS),
        # RUNNER (meta) params — sampling knobs saved on the record, distinct from
        # connection (endpoint/key/model). Merged verbatim into the request body.
        sampling=rec.get("sampling"),
    )


def _build():
    from responses_backend.provider import DEFAULT_ENDPOINT, DEFAULT_MODEL

    return build(
        sentence="OpenAI Responses-API LLM agent (raw prompt-and-parse tool-calling).",
        default_model=DEFAULT_MODEL,
        default_endpoint=DEFAULT_ENDPOINT,
        make_provider=make_provider,
        name="responses_backend",
        module_name=__name__,
    )


VERBS, handler = _build()
