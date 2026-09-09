"""THE PLUG POINT — one definition of "which model, reached how".

Every probe used to carry its own `if BACKEND == "anthropic": … elif …` in two
or three places (the record meta, the provider import, the model default). Ten
probes × three sites is thirty branches that drift independently, and they did:
one probe never read `ENDPOINT` and so died on `anthropic 401` while its five
siblings ran fine against the same proxy (measured 2026-08-24).

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
    #: record fields beyond `handler_module`/`model` (endpoint, keys, sidecars)
    extra: dict[str, Any] = field(default_factory=dict)
    #: env vars that must be set, with the reason, checked before any turn
    requires: tuple[tuple[str, str], ...] = ()
    #: env var the BACKEND ITSELF reads its key from. Nothing delivers it — the
    #: substrate keeps keys out of agent meta and off disk on purpose
    api_key_var: str | None = None
    #: does this backend need a file-bridge sidecar to read/write files? Asked
    #: as a capability so a probe never branches on the backend NAME.
    needs_file_bridge: bool = False
    #: does this plug take a per-agent `think` record field? Only the local
    #: servers gate thinking that way; the hosted routes decide it themselves.
    #: Asked as a capability for the same reason as the line above.
    supports_think: bool = False

    def record(self, **overrides: Any) -> dict:
        """The record fields for `create_agent`. Merged over by the probe's own
        wiring (grants, ids), never the reverse."""
        meta: dict[str, Any] = {
            "handler_module": self.handler_module,
            # A plug that serves one model names none — see `_llamacpp`.
            **({"model": self.model} if self.model else {}),
            **self.extra,
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


class _Anthropic(Backend):
    async def provider(self, agent_id: str, root):
        import anthropic_backend.tools as at

        return at.make_provider(agent_id, root)


class _Nvidia(Backend):
    async def provider(self, agent_id: str, root):
        import nvidia_nim_backend.tools as nt

        return await nt.make_provider(agent_id, root)


class _Ollama(Backend):
    async def provider(self, agent_id: str, root):
        import ollama_backend.tools as ot

        return ot.make_provider(agent_id, root)


class _LlamaCpp(Backend):
    async def provider(self, agent_id: str, root):
        import llamacpp_backend.tools as lt

        return lt.make_provider(agent_id, root)


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


# RUNNER (meta) params — sampling/budget knobs merged verbatim into the request body,
# distinct from CONNECTION (endpoint/key/model). They live in JSON beside this file,
# NOT baked here: `sampling.default.json` (committed defaults) overlaid by
# `sampling.json` (gitignored local override). The endpoint whitelists these and
# silently drops anything unlisted. No model-specific values in code.
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


def _anthropic() -> Backend:
    extra: dict[str, Any] = {}
    # WITHOUT this the record falls back to api.anthropic.com while carrying
    # whatever key the run exported — a 401 the moment the key belongs to a
    # different host. That is not hypothetical: it killed one probe at pass 1
    # while five siblings ran fine, because it was the only harness that never
    # read ENDPOINT.
    if ep := os.environ.get("ENDPOINT"):
        extra["endpoint"] = ep
    extra["max_tokens"] = int(os.environ.get("MAX_TOKENS") or THINK_BUDGET)
    # Runner (sampling/budget) params from the JSON beside this file — see above.
    if sampling := _runner_params():
        extra["sampling"] = sampling
    return _Anthropic(
        name="anthropic",
        handler_module="anthropic_backend.tools",
        model=_model_or_die("anthropic"),
        extra=extra,
        requires=(
            (
                "ANTHROPIC_API_KEY",
                "the backend refuses to send without one, even where the proxy "
                "ignores its value (use `keyless` against a keyless proxy)",
            ),
        ),
    )


def _nvidia() -> Backend:
    return _Nvidia(
        name="nvidia",
        handler_module="nvidia_nim_backend.tools",
        model=_model_or_die("nvidia"),
        api_key_var="NVIDIA_API_KEY",
        needs_file_bridge=True,
        requires=(("NVIDIA_API_KEY", "NIM authenticates every request"),),
    )


def _ollama() -> Backend:
    return _Ollama(
        name="ollama",
        handler_module="ollama_backend.tools",
        model=_model_or_die("ollama"),
    )


def _llamacpp() -> Backend:
    return _LlamaCpp(
        name="llamacpp",
        handler_module="llamacpp_backend.tools",
        # llama-server serves exactly ONE model (whichever -hf/-m loaded) and
        # IGNORES the request's `model` field. There is nothing to name here, so
        # nothing is demanded — and `record()` drops the empty value rather than
        # sending a field the server would discard.
        model=os.environ.get("FANTASTIC_AI_MODEL", ""),
        supports_think=True,
        # The server's URL is the operator's to name. It used to default to a
        # loopback host here — the one guessed endpoint left in the plug that
        # says "NO DEFAULTS THAT REACH A NETWORK" (removed 2026-09-02).
        requires=(
            (
                "FANTASTIC_LLAMACPP_ENDPOINT",
                "the llama-server /v1 URL; no host is guessed",
            ),
        ),
        # `think` is a llama.cpp-specific record field and is NOT defaulted here.
        # gemma-4 gates thinking behind `enable_thinking` (its chat template
        # defaults it false) and emits `<|channel>thought` rather than `<think>`
        # tags; a server that sets neither produces the measured failure — thinking
        # text present, `content` EMPTY. Which way that should go is the operator's
        # call per model, so it is sent only when `THINK` is explicitly set.
        extra={
            **(
                {"endpoint": ep}
                if (ep := os.environ.get("FANTASTIC_LLAMACPP_ENDPOINT"))
                else {}
            ),
            **({"think": os.environ["THINK"] == "1"} if "THINK" in os.environ else {}),
        },
    )


REGISTRY = {
    "anthropic": _anthropic,
    "nvidia": _nvidia,
    "ollama": _ollama,
    "llamacpp": _llamacpp,
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
