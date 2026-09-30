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

from responses_backend.provider import KEY_VAR, MAX_TOKENS

NAME = "responses_backend"

#: THE RECORD SCHEMA (lib request #3) — what this kind's record takes, read with
#: no agent existing (`reflect bundles=all`), from the constants the provider runs
#: on. `validate_meta` enforces it at the write, so the page and the kernel cannot
#: disagree about what a record needs.
RECORD_SCHEMA = schema(
    [
        field(
            "endpoint",
            "str",
            "the full Responses API URL (…/v1/responses)",
            required=True,
        ),
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
    """Build a ResponsesProvider from the agent record (endpoint/model/max_tokens/sampling)."""
    from responses_backend.provider import ResponsesProvider

    rec = kernel.get(id) or {}
    return ResponsesProvider(
        endpoint=required_meta(rec, "endpoint", NAME),
        model=required_meta(rec, "model", NAME),
        max_tokens=int(rec.get("max_tokens") or MAX_TOKENS),
        # RUNNER (meta) params — sampling knobs saved on the record, distinct from
        # connection (endpoint/key/model). Merged verbatim into the request body.
        sampling=rec.get("sampling"),
    )


def _build():

    return build(
        sentence="OpenAI Responses-API LLM agent (raw prompt-and-parse tool-calling).",
        make_provider=make_provider,
        name=NAME,
        module_name=__name__,
    )


VERBS, handler = _build()
