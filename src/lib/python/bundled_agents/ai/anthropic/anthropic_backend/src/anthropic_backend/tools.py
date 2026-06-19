"""anthropic bundle — reflect-driven LLM agent with native tool-calling.

A THIN binding over `ai_core` (see ollama_backend.tools for the pattern):
this module supplies the AnthropicProvider builder + the backend's
identity, and `ai_core.build()` returns the `(VERBS, handler)` bound to
it. ALL shared machinery (queue / FIFO lock / menu cache, prompt
assembly, the agentic `_run` loop, the verb bodies) lives in
`ai_core.core`. State dicts + the patchable `SEND_TIMEOUT` /
`MAX_CALL_DEPTH` are re-exported from `ai_core.core` so any test
monkeypatch seam keeps working on the shared core path.
"""

from __future__ import annotations

from ai_core import build
from ai_core.core import (  # noqa: F401 — re-exported test seams
    DEFAULT_CLIENT_ID,
    MAX_CALL_DEPTH,
    SEND_TIMEOUT,
    SEND_TOOL,
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
    """Build an AnthropicProvider from the agent record (endpoint/model)."""
    from anthropic_backend.provider import (
        DEFAULT_ENDPOINT,
        DEFAULT_MODEL,
        AnthropicProvider,
    )

    rec = kernel.get(id) or {}
    return AnthropicProvider(
        endpoint=rec.get("endpoint", DEFAULT_ENDPOINT),
        model=rec.get("model", DEFAULT_MODEL),
    )


def _build():
    from anthropic_backend.provider import DEFAULT_ENDPOINT, DEFAULT_MODEL

    return build(
        sentence="Anthropic-backed LLM agent (native tool-calling).",
        default_model=DEFAULT_MODEL,
        default_endpoint=DEFAULT_ENDPOINT,
        make_provider=make_provider,
        name="anthropic_backend",
        module_name=__name__,
    )


VERBS, handler = _build()
