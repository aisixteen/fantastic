"""ollama bundle — a bare, context-managed LLM completion agent.

A THIN binding over `ai_core`: this module supplies the OllamaProvider
builder + the backend's identity, and `ai_core.build()` returns the
`(VERBS, handler)` bound to it. ALL the shared machinery (queue / FIFO
lock, prompt assembly, the ONE-PASS `_run`, context management, the verb
bodies) lives in `ai_core.core`. There is NO agentic loop — tool dispatch
is the caller's job (an authored agent); this backend does one provider pass per send.

The state dicts (`_providers`, `_queue`, `_current`, `_locks`) and the
patchable `SEND_TIMEOUT` / `MAX_CALL_DEPTH` constants are RE-EXPORTED from
`ai_core.core` here so the existing tests' monkeypatch seams
(`ollama_backend.tools._providers[id] = fake`,
`monkeypatch.setattr(..., "SEND_TIMEOUT", 0.05)`, calls to `_assemble`)
keep working unchanged on the shared core path. The re-exported dicts are
the SAME objects `ai_core.core` mutates.

`on_cancel` is re-exported the same way — that module-level name IS the
bundle's cancel hook (the substrate discovers it here, exactly like
`on_delete`), so this backend inherits ai_core's shared implementation
without restating it. There is no `interrupt` verb: an in-flight `send` is
a kernel TASK, killed with the universal `ps` → `cancel` system verbs.
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

from ollama_backend.provider import DEFAULT_TEMPERATURE

NAME = "ollama_backend"

#: THE RECORD SCHEMA (lib request #3) — what this kind's record takes, read with
#: no agent existing (`reflect bundles=all`), from the constants the provider runs
#: on. `validate_meta` enforces it at the write, so the page and the kernel cannot
#: disagree about what a record needs.
RECORD_SCHEMA = schema(
    [
        field("endpoint", "str", "the ollama server URL", required=True),
        field("model", "str", "the model name", required=True),
        field("num_ctx", "int", "the context window ollama loads the model with"),
        field(
            "temperature", "float", "sampling temperature", default=DEFAULT_TEMPERATURE
        ),
        field("num_predict", "int", "a turn's output budget (tokens)"),
        field("think", "bool", "turn the model's thinking channel on or off"),
    ],
)


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse, AT THE WRITE, a record missing a required field or naming an enum
    value outside its set — every missing field named at once, from the schema."""
    return validate(RECORD_SCHEMA, NAME, meta, record)


def make_provider(id, kernel):
    """Build an OllamaProvider from the agent record
    (endpoint/model/num_ctx/temperature/num_predict)."""
    from ollama_backend.provider import (
        OllamaProvider,
    )

    rec = kernel.get(id) or {}
    num_ctx = rec.get("num_ctx")
    temperature = rec.get("temperature")
    num_predict = rec.get("num_predict")
    think = rec.get("think")
    return OllamaProvider(
        endpoint=required_meta(rec, "endpoint", NAME),
        model=required_meta(rec, "model", NAME),
        num_ctx=int(num_ctx) if num_ctx else None,
        temperature=float(temperature) if temperature is not None else None,
        num_predict=int(num_predict) if num_predict else None,
        think=bool(think) if think is not None else None,
    )


async def _reflect_extra(id, kernel):
    """Merged into reflect: the ollama sampling knobs, so `reflect` shows the
    effective posture (parity with the rust ollama backend). `num_ctx`/
    `num_predict`/`think` are null unless set on the record; temperature defaults
    to the provider's 0.3."""
    rec = kernel.get(id) or {}
    t = rec.get("temperature")
    return {
        "temperature": float(t) if t is not None else DEFAULT_TEMPERATURE,
        "num_ctx": int(rec["num_ctx"]) if rec.get("num_ctx") else None,
        "num_predict": int(rec["num_predict"]) if rec.get("num_predict") else None,
        "think": bool(rec["think"]) if rec.get("think") is not None else None,
    }


def _build():

    return build(
        sentence="Ollama-backed LLM agent (raw prompt-and-parse tool-calling).",
        make_provider=make_provider,
        name=NAME,
        module_name=__name__,
        reflect_extra=_reflect_extra,
    )


VERBS, handler = _build()
