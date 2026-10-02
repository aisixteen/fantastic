"""THE PLUG POINT — one definition of "which model, reached how".

Every probe used to carry its own `if BACKEND == …: … elif …` in two or three
places (the record meta, the provider import, the model default). Ten probes ×
three sites is thirty branches that drift independently, and they did: one probe
never read its endpoint variable and so died on a 401 while its five siblings ran
fine against the same server (measured 2026-08-24).

So the choice lives HERE, once. A probe asks for `backend()` and gets an object
that knows how to describe itself as a record, how to build its provider, and
what it needs before it can run. Adding a backend is adding a file, not editing
ten probes.

NO DEFAULTS THAT REACH A NETWORK. The substrate refuses to guess a model or an
endpoint and so does this: an unset `FANTASTIC_AI_MODEL` is an error naming the
variable, never a guess that quietly measures the wrong model.
"""

from __future__ import annotations

import os

from helpers.creds import KEYED, credentials

#: THE THINK BUDGET — one constant, every test, every runtime.
#:
#: `max_tokens` is a property of the MODEL PLUG, not of a test: it bounds one
#: completion (thinking + text), and a test has no way to know what its ask costs
#: in tokens. It used to be a per-test knob, and 25 files carried their own value
#: — 4096, 8192, 16384, 32768 — set by whoever last watched a lane truncate.
#: `E32_graph_shape` had none at all, so it silently ran at the backend's 8192
#: while the sibling it is the A/B CONTROL for ran at 32768: the two differed in
#: think budget as well as in the ask, and that comparison was never valid
#: (measured 2026-09-03). A budget nobody can see is a budget that decides
#: results by accident.
#:
#: THIS IS THE OUTPUT RESERVE, NOT THE WINDOW. The endpoint enforces
#: `max_tokens + prompt <= context` (262144 here), so the cap is a JOINT budget:
#: at 260_000 only 2144 tokens are left for the prompt and EVERY turn 400s on the
#: first send —
#:   "you requested 260000 output tokens and your prompt contains at least 2145
#:    input tokens, for a total of at least 262145"
#: (measured 2026-09-03). A one-shot probe with a three-word prompt accepts
#: 260_000, which is why the window looks like a valid cap and is not.
#:
#: 65536 sits comfortably above anything observed — the largest legitimate
#: authoring turn measured ~34k CHARACTERS, on the order of 9k tokens — and still
#: leaves ~196k for the conversation, more prompt room than any lane has used.
#: The env var overrides for one deliberate run; nothing declares it per test.
THINK_BUDGET = 65_536
from dataclasses import dataclass, field
from typing import Any


@dataclass
class Backend:
    """One inference plug: how to CREATE it, how to BUILD its provider, and what
    it needs first."""

    name: str
    handler_module: str
    model: str
    #: record fields beyond `handler_module`/`model` (endpoint, sidecars)
    extra: dict[str, Any] = field(default_factory=dict)
    #: env vars that must be set, with the reason, checked before any turn
    requires: tuple[tuple[str, str], ...] = ()
    #: the ref this plug's key is read from in the run's `creds` store
    #: (`helpers/creds.py`), or None for a keyless plug. The backend never reads
    #: the environment: the STORE does (env backend), at use time.
    key_ref: str | None = None

    def record(self, **overrides: Any) -> dict:
        """The record fields for `create_agent`. Merged over by the probe's own
        wiring (grants, ids), never the reverse."""
        meta: dict[str, Any] = {
            "handler_module": self.handler_module,
            "model": self.model,
            **self.extra,
            # THE KEY, by reference: the backend reads it from `creds` per request.
            **({"credentials": credentials(self.key_ref)} if self.key_ref else {}),
        }
        meta.update(overrides)
        return meta

    def blockers(self) -> list[str]:
        """What would make a run fail for a reason that is NOT the substrate."""
        out = []
        for var, why in self.requires:
            if not os.environ.get(var):
                out.append(f"{var} is unset — {why}")
        return out

    async def provider(self, agent_id: str, root):
        """Build the live provider for an already-created agent."""
        raise NotImplementedError


class _Ollama(Backend):
    async def provider(self, agent_id: str, root):
        import ollama_backend.tools as ot

        return ot.make_provider(agent_id, root)


class _Responses(Backend):
    async def provider(self, agent_id: str, root):
        import responses_backend.tools as rt

        return rt.make_provider(agent_id, root)


class _Vllm(Backend):
    async def provider(self, agent_id: str, root):
        import vllm_backend.tools as vt

        return vt.make_provider(agent_id, root)


def _model_or_die(name: str) -> str:
    m = os.environ.get("FANTASTIC_AI_MODEL")
    if not m:
        raise SystemExit(
            f"FANTASTIC_AI_MODEL is unset. The substrate refuses to guess a model "
            f"and so does this launcher — a guessed default is how a run ends up "
            f"measuring something other than what you meant. Set it, e.g.\n"
            f"  FANTASTIC_AI_MODEL=<model {name} serves>"
        )
    return m


# RUNNER (meta) params — sampling knobs, distinct from CONNECTION (endpoint/key/
# model). They live in JSON beside this file, NOT baked here: `sampling.default.json`
# (committed defaults) overlaid by `sampling.json` (gitignored local override). The
# vllm plug copies the ones vllm_backend takes as RECORD fields (see `_vllm`). No
# model-specific values in code.
def _runner_params() -> dict[str, Any]:
    import json

    here = os.path.dirname(__file__)
    out: dict[str, Any] = {}
    for fn in ("sampling.default.json", "sampling.json"):  # local overrides defaults
        path = os.path.join(here, fn)
        if not os.path.exists(path):
            continue
        try:
            data = json.load(open(path))
        except (ValueError, OSError):
            continue
        out.update({k: v for k, v in data.items() if not k.startswith("_")})
    return {k: v for k, v in out.items() if v is not None}


def _ollama() -> Backend:
    return _Ollama(
        name="ollama",
        handler_module="ollama_backend.tools",
        model=_model_or_die("ollama"),
    )


def _responses() -> Backend:
    # The OpenAI Responses wire (`…/v1/responses`). Its URL is the operator's to
    # name — nothing guessed, the plug refuses to run without it.
    extra: dict[str, Any] = {"max_tokens": int(os.environ.get("MAX_TOKENS") or THINK_BUDGET)}
    if ep := os.environ.get("RESPONSES_ENDPOINT"):
        extra["endpoint"] = ep
    # NO knobs from the JSON beside this file: `no_repeat_ngram_size` and
    # `repetition_penalty` are vLLM sampling fields, not Responses fields. A
    # Responses run that wants a knob names it in its own record.
    return _Responses(
        name="responses",
        handler_module="responses_backend.tools",
        model=_model_or_die("responses"),
        extra=extra,
        key_ref="responses",
        requires=(
            ("RESPONSES_ENDPOINT", "the full …/v1/responses URL; no host is guessed"),
            (KEYED["responses"][0], "the `creds` store reads the key from it"),
        ),
    )


def _vllm() -> Backend:
    # A vLLM server reached DIRECTLY (no proxy) — its `/v1` URL, the served
    # model, and the KV-cache session table's size and rotation policy are all
    # the operator's to name: the backend requires every one of them, and so
    # does this plug. For the shared dataroom GPU the URL is its HTTPS door, which
    # wants a key (recipe: tmp/vllm_backend.md) — keep lanes at 1–2 there, the
    # server's running slots are shared with production. THE KEY is optional on
    # a vLLM record: a run that exports `VLLM_API_KEY` names the `vllm` ref, one
    # that does not names none and the server is reached keyless.
    extra: dict[str, Any] = {"max_tokens": int(os.environ.get("MAX_TOKENS") or THINK_BUDGET)}
    if ep := os.environ.get("VLLM_ENDPOINT"):
        extra["endpoint"] = ep
    if slots := os.environ.get("VLLM_KV_SLOTS"):
        extra["kv_slots"] = int(slots)
    if overflow := os.environ.get("VLLM_KV_OVERFLOW"):
        extra["kv_overflow"] = overflow
    # The runner knobs from the JSON beside this file: `no_repeat_ngram_size`
    # and `repetition_penalty` are the measured fix for the CoT-repetition loop
    # (verbatim-repeat rate 48x -> 0). vllm_backend takes them as RECORD fields.
    #
    # `thinking_token_budget` is left to the backend's own default ON PURPOSE:
    # on the record it is EXPLICIT, and an explicit budget refuses any call it
    # could not fire on — every fold summary is bounded at its budget + 1024
    # tokens, below 8000, so all folds would be refused. The default adapts per
    # call instead (min(16000, max_tokens // 2)).
    runner = _runner_params()
    for knob in ("no_repeat_ngram_size", "repetition_penalty"):
        if knob in runner:
            extra[knob] = runner[knob]
    return _Vllm(
        name="vllm",
        handler_module="vllm_backend.tools",
        model=_model_or_die("vllm"),
        extra=extra,
        key_ref="vllm" if os.environ.get(KEYED["vllm"][0]) else None,
        requires=(
            ("VLLM_ENDPOINT", "the vLLM /v1 URL; no host is guessed"),
            ("VLLM_KV_SLOTS", "how many conversations may be live on the endpoint"),
            ("VLLM_KV_OVERFLOW", "evict (rotate the LRU out) or reject (refuse the newcomer)"),
        ),
    )


class _FoundationModels(Backend):
    """Apple's on-device model (rust + swift only — python has no FM backend). Its
    record names no model, endpoint or key: the backend refuses them."""

    def record(self, **overrides: Any) -> dict:
        meta: dict[str, Any] = {"handler_module": self.handler_module, **self.extra}
        meta.update(overrides)
        return meta

    async def provider(self, agent_id: str, root):
        raise NotImplementedError("python has no foundation_models backend — run with --runtime rust|swift")


def _foundation_models() -> Backend:
    return _FoundationModels(name="foundation_models", handler_module="foundation_models_backend.tools", model="")


REGISTRY = {
    "ollama": _ollama,
    "responses": _responses,
    "vllm": _vllm,
    "foundation_models": _foundation_models,
}


def backend(name: str | None = None) -> Backend:
    """Resolve the backend. NAMED explicitly or via `BACKEND` — never guessed.

    Probes used to default to `ollama`, which is not installed here; the failure
    then looked like a substrate fault instead of an unset variable.
    """
    name = name or os.environ.get("BACKEND")
    if not name:
        raise SystemExit("BACKEND is unset. Name it: " + " | ".join(sorted(REGISTRY)))
    if name not in REGISTRY:
        raise SystemExit(
            f"unknown BACKEND {name!r}; known: {' | '.join(sorted(REGISTRY))}"
        )
    return REGISTRY[name]()
