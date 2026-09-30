"""anthropic bundle — reflect-driven LLM agent with native tool-calling.

A THIN binding over `ai_core` (see ollama_backend.tools for the pattern):
this module supplies the AnthropicProvider builder + the backend's
identity, and `ai_core.build()` returns the `(VERBS, handler)` bound to
it. ALL shared machinery (queue / FIFO lock / menu cache, prompt
assembly, the agentic `_run` loop, the verb bodies) lives in
`ai_core.core`. State dicts + the patchable `SEND_TIMEOUT` /
`MAX_CALL_DEPTH` are re-exported from `ai_core.core` so any test
monkeypatch seam keeps working on the shared core path — and so is
`on_cancel`, the shared bundle CANCEL hook the substrate discovers on this
module (there is no `interrupt` verb: an in-flight `send` is a kernel TASK,
killed with the universal `ps` → `cancel` system verbs).
"""

from __future__ import annotations

from ai_core import build, required_meta
from ai_core.schema import field, schema, validate
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

from anthropic_backend.provider import KEY_VAR, MAX_TOKENS

NAME = "anthropic_backend"

#: THE RECORD SCHEMA (lib request #3) — what this kind's record takes, read with
#: no agent existing (`reflect bundles=all`), from the constants the provider runs
#: on. `validate_meta` enforces it at the write, so the page and the kernel cannot
#: disagree about what a record needs.
RECORD_SCHEMA = schema(
    [
        field("endpoint", "str", "the Messages API URL (…/v1/messages)", required=True),
        field("model", "str", "the model name", required=True),
        field(
            "max_tokens", "int", "a turn's output budget (tokens)", default=MAX_TOKENS
        ),
        field("sampling", "map", "runner params merged verbatim into the request body"),
    ],
    key_var=KEY_VAR,
)


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse, AT THE WRITE, a record missing a required field or naming an enum
    value outside its set — every missing field named at once, from the schema."""
    return validate(RECORD_SCHEMA, NAME, meta, record)


def make_provider(id, kernel):
    """Build an AnthropicProvider from the agent record (endpoint/model)."""
    from anthropic_backend.provider import AnthropicProvider

    rec = kernel.get(id) or {}
    return AnthropicProvider(
        endpoint=required_meta(rec, "endpoint", NAME),
        model=required_meta(rec, "model", NAME),
        max_tokens=int(rec.get("max_tokens") or MAX_TOKENS),
        # RUNNER (meta) params — sampling/budget knobs saved on the record, distinct
        # from connection (endpoint/key/model). Merged verbatim into the request body.
        sampling=rec.get("sampling"),
    )


def _build():

    return build(
        sentence="Anthropic-backed LLM agent (raw prompt-and-parse tool-calling).",
        make_provider=make_provider,
        name=NAME,
        module_name=__name__,
    )


VERBS, handler = _build()
