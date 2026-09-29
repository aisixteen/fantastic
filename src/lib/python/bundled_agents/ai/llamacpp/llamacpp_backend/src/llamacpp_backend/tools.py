"""llamacpp bundle — a bare, context-managed LLM completion agent.

A THIN binding over `ai_core`: this module supplies the LlamaCppProvider
builder + the backend's identity, and `ai_core.build()` returns the
`(VERBS, handler)` bound to it. ALL the shared machinery (queue / FIFO
lock, prompt assembly, the ONE-PASS `_run`, context management, the verb
bodies) lives in `ai_core.core`. There is NO agentic loop — tool dispatch
is the caller's job (an authored agent); this backend does one provider pass
per send.

Distinct from `nvidia_nim_backend` on purpose even though both speak the
OpenAI-compatible wire: a `reflect` that described a local gemma as an NVIDIA
NIM agent would put a LIE in the tree, and self-description is the whole point.

The state dicts (`_providers`, `_queue`, `_current`, `_locks`) and the
patchable `SEND_TIMEOUT` / `MAX_CALL_DEPTH` constants are RE-EXPORTED from
`ai_core.core` here so the monkeypatch seams used by the other backends'
tests work identically on this one. The re-exported dicts are the SAME
objects `ai_core.core` mutates.

`on_cancel` is re-exported the same way — that module-level name IS the
bundle's cancel hook (the substrate discovers it here, exactly like
`on_delete`). There is no `interrupt` verb: an in-flight `send` is a kernel
TASK, killed with the universal `ps` → `cancel` system verbs.
"""

from __future__ import annotations

from ai_core import build, required_meta
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
    """Build a LlamaCppProvider from the agent record
    (endpoint/model/api_key/temperature/num_predict/think)."""
    from llamacpp_backend.provider import LlamaCppProvider

    rec = kernel.get(id) or {}
    temperature = rec.get("temperature")
    num_predict = rec.get("num_predict")
    think = rec.get("think")
    return LlamaCppProvider(
        endpoint=required_meta(rec, "endpoint", "llamacpp_backend"),
        model=rec.get("model"),
        api_key=rec.get("api_key"),
        temperature=float(temperature) if temperature is not None else None,
        num_predict=int(num_predict) if num_predict else None,
        think=bool(think) if think is not None else None,
    )


async def _reflect_extra(id, kernel):
    """Merged into reflect: the sampling knobs, so `reflect` shows the effective
    posture. `num_predict`/`think` are null unless set on the record; temperature
    defaults to the provider's 0.3. `num_ctx` is deliberately ABSENT — llama-server
    fixes the context window at start (`-c N`) and there is no per-request
    equivalent, so this agent does not claim to own one."""
    rec = kernel.get(id) or {}
    t = rec.get("temperature")
    return {
        "temperature": float(t) if t is not None else 0.3,
        "num_predict": int(rec["num_predict"]) if rec.get("num_predict") else None,
        "think": bool(rec["think"]) if rec.get("think") is not None else None,
        "has_api_key": bool(rec.get("api_key")),
    }


def _build():

    return build(
        sentence="llama.cpp-backed local LLM agent (raw prompt-and-parse tool-calling).",
        make_provider=make_provider,
        name="llamacpp_backend",
        module_name=__name__,
        reflect_extra=_reflect_extra,
    )


VERBS, handler = _build()
