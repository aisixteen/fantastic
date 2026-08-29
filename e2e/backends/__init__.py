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


def _anthropic() -> Backend:
    extra: dict[str, Any] = {}
    # WITHOUT this the record falls back to api.anthropic.com while carrying
    # whatever key the run exported — a 401 the moment the key belongs to a
    # different host. That is not hypothetical: it killed one probe at pass 1
    # while five siblings ran fine, because it was the only harness that never
    # read ENDPOINT.
    if ep := os.environ.get("ENDPOINT"):
        extra["endpoint"] = ep
    # Declaring the one `send` tool is a REQUEST-side choice and off by default:
    # tool-calling here is raw prompt-and-parse. It is not needed to READ what a
    # `/cc`-style route returns unasked — the provider translates those blocks to
    # the raw envelope regardless.
    if os.environ.get("TOOL_USE") == "1":
        extra["tool_use"] = True
    if mt := int(os.environ.get("MAX_TOKENS", "0")):
        extra["max_tokens"] = mt
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
        # `think` is a llama.cpp-specific record field and is NOT defaulted here.
        # gemma-4 gates thinking behind `enable_thinking` (its chat template
        # defaults it false) and emits `<|channel>thought` rather than `<think>`
        # tags; a server that sets neither produces the measured failure — thinking
        # text present, `content` EMPTY. Which way that should go is the operator's
        # call per model, so it is sent only when `THINK` is explicitly set.
        extra={
            "endpoint": os.environ.get(
                "FANTASTIC_LLAMACPP_ENDPOINT", "http://127.0.0.1:8080/v1"
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
        raise SystemExit(
            "BACKEND is unset. Name it: " + " | ".join(sorted(REGISTRY))
        )
    if name not in REGISTRY:
        raise SystemExit(
            f"unknown BACKEND {name!r}; known: {' | '.join(sorted(REGISTRY))}"
        )
    return REGISTRY[name]()
