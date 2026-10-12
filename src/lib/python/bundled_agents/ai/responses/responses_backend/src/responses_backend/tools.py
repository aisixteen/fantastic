"""responses bundle — reflect-driven LLM agent over the OpenAI Responses API.

A THIN binding over `ai_core` (see ollama_backend.tools for the pattern): this
module supplies the ResponsesProvider builder + the backend's identity, and
`ai_core.build()` returns the `(VERBS, handler)` bound to it. ALL shared machinery
(queue / FIFO lock, prompt assembly, the ONE-PASS `_run`, context management, the
verb bodies) lives in `ai_core.core`. `on_cancel` and `on_delete` are re-exported
from `ai_core` — the shared bundle CANCEL and DELETE hooks the substrate discovers
on this module (an in-flight `send` is a kernel TASK, killed with the universal
`ps` → `cancel` system verbs).

Distinct from `vllm_backend` on purpose: that one speaks OpenAI Chat Completions
(`/chat/completions`), this one the Responses wire (`/responses`) — a different
request shape and a different event stream.
"""

from __future__ import annotations

from ai_core import build, on_cancel, on_delete, required_meta  # noqa: F401 — the hooks
from ai_core.credentials import key_reader
from ai_core.schema import credentials_field, field, schema, validate

from responses_backend.provider import MAX_TOKENS

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
        credentials_field(required=True),
    ],
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
        key=key_reader(id, kernel, NAME, required=True),
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
    )


VERBS, handler = _build()

# Served by the kernel's `docs` verb (tier detailed) — shared by every backend.
from ai_core import CANCEL, EMITS, NOTES  # noqa: E402, F401
