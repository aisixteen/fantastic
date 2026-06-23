"""ollama bundle — reflect-driven LLM agent with native tool-calling.

A THIN binding over `ai_core`: this module supplies the OllamaProvider
builder + the backend's identity, and `ai_core.build()` returns the
`(VERBS, handler)` bound to it. ALL the shared machinery (queue / FIFO
lock / menu cache, prompt assembly, the agentic `_run` loop, the verb
bodies) lives in `ai_core.core`.

The state dicts (`_providers`, `_queue`, `_current`, `_menu_cache`,
`_tasks`, `_locks`) and the patchable `SEND_TIMEOUT` / `MAX_CALL_DEPTH`
constants are RE-EXPORTED from `ai_core.core` here so the existing tests'
monkeypatch seams (`ollama_backend.tools._providers[id] = fake`,
`monkeypatch.setattr(..., "SEND_TIMEOUT", 0.05)`, calls to `_assemble` /
`_invalidate_menu`) keep working unchanged on the shared core path. The
re-exported dicts are the SAME objects `ai_core.core` mutates.
"""

from __future__ import annotations

from ai_core import build
from ai_core.core import (  # noqa: F401 — re-exported test seams
    DEFAULT_CLIENT_ID,
    MAX_CALL_DEPTH,
    SEND_TIMEOUT,
    _assemble,
    _build_menu,
    _current,
    _invalidate_menu,
    _locks,
    _menu_cache,
    _providers,
    _queue,
    _tasks,
)


def make_provider(id, kernel):
    """Build an OllamaProvider from the agent record (endpoint/model/num_ctx/temperature)."""
    from ollama_backend.provider import (
        DEFAULT_ENDPOINT,
        DEFAULT_MODEL,
        OllamaProvider,
    )

    rec = kernel.get(id) or {}
    num_ctx = rec.get("num_ctx")
    temperature = rec.get("temperature")
    return OllamaProvider(
        endpoint=rec.get("endpoint", DEFAULT_ENDPOINT),
        model=rec.get("model", DEFAULT_MODEL),
        num_ctx=int(num_ctx) if num_ctx else None,
        temperature=float(temperature) if temperature is not None else None,
    )


def _build():
    from ollama_backend.provider import DEFAULT_ENDPOINT, DEFAULT_MODEL

    return build(
        sentence="Ollama-backed LLM agent (raw prompt-and-parse tool-calling).",
        default_model=DEFAULT_MODEL,
        default_endpoint=DEFAULT_ENDPOINT,
        make_provider=make_provider,
        name="ollama_backend",
        module_name=__name__,
    )


VERBS, handler = _build()
