"""anthropic bundle — an LLM agent over Anthropic's Messages API.

A THIN binding over `ai_core`, like every backend: this module supplies the
`AnthropicProvider` builder, the write-time checks and the backend's identity, and
`ai_core.build()` returns the `(VERBS, handler)` bound to it. There is NO agentic
loop — one provider pass per send, and tool-calling is RAW: no `tools` array, the
`<call>` elements are ai_core's to parse.

What the record leaves out of `max_tokens` / `context_window` is read from the API
before a turn and written back (`server.py`). The key is optional on the record:
`credentials`, a
credential-store ref, sent as `x-api-key` when named.

`on_cancel` / `on_delete` are re-exported from `ai_core` — the bundle's hooks.
"""

from __future__ import annotations

from ai_core import build, on_cancel, required_meta  # noqa: F401 — the hooks
from ai_core import on_delete as _core_on_delete
from ai_core.credentials import key_reader
from ai_core.schema import credentials_field, field, schema, validate
from kernel._refusal import BAD_ARG, refusal

from anthropic_backend import server
from anthropic_backend.provider import THINKING_FLOOR, Usage

NAME = "anthropic_backend"

#: Each agent's token counts, keyed by id. RAM: a kernel restart zeroes them.
_USAGE: dict[str, Usage] = {}


def usage_of(id: str) -> Usage:
    """The agent's running counts — created on first use."""
    if id not in _USAGE:
        _USAGE[id] = Usage()
    return _USAGE[id]


_FROM_API = "; left out, read from the API before a turn and written here"

#: THE RECORD SCHEMA — what this kind's record takes, read with no agent existing
#: (`reflect bundles=all`); `validate_meta` enforces it at the write. `endpoint` and
#: `model` are required (the API serves many models); the counts a record leaves
#: out are read from the API's model entry, never given a default.
RECORD_SCHEMA = schema(
    [
        field(
            "endpoint",
            "str",
            "the Messages API URL, e.g. https://api.anthropic.com/v1/messages",
            required=True,
        ),
        field("model", "str", "the model id, e.g. claude-sonnet-5", required=True),
        field(
            "max_tokens",
            "int",
            "a turn's output budget (tokens)" + _FROM_API + " (the model's max_tokens)",
        ),
        field(
            "context_window",
            "int",
            "the model's input window (tokens)" + _FROM_API + " (max_input_tokens)",
        ),
        field("temperature", "float", "sampling temperature; sent only when set"),
        field(
            "thinking_budget",
            "int",
            f"extended thinking per turn (tokens, at least {THINKING_FLOOR}, below "
            "max_tokens); unset, no thinking is asked for",
        ),
        field(
            "prompt_cache",
            "bool",
            "prompt caching, opt-in: the system block and the conversation tail carry "
            "a 5-minute cache marker; unset or false, the request is unchanged",
        ),
        field(
            "price_input_per_mtok",
            "float",
            "USD per million input tokens — with price_output_per_mtok, reflect and "
            "usage report cost_usd and saved_usd; no default price",
        ),
        field(
            "price_output_per_mtok",
            "float",
            "USD per million output tokens — with price_input_per_mtok, reflect and "
            "usage report cost_usd and saved_usd; no default price",
        ),
        # the API key — optional on the record: absent, no key header goes out
        credentials_field(required=False),
    ],
)


def _positive_int(v) -> bool:
    return isinstance(v, int) and not isinstance(v, bool) and v >= 1


def _number(v) -> bool:
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse a write this backend cannot honour, AT THE WRITE, on the MERGED
    record — then every required field, named at once, from the schema."""
    merged = {**record, **meta}
    for key in ("max_tokens", "context_window", "thinking_budget"):
        if key in meta and meta[key] is not None and not _positive_int(meta[key]):
            return f"{key} must be a positive int"
    t = meta.get("temperature")
    if t is not None and not _number(t):
        return "temperature must be a number"
    c = meta.get("prompt_cache")
    if c is not None and not isinstance(c, bool):
        return "prompt_cache must be a boolean"
    for key in ("price_input_per_mtok", "price_output_per_mtok"):
        p = meta.get(key)
        if p is not None and (not _number(p) or p < 0):
            return f"{key} must be a number, zero or more"
    budget, mx = merged.get("thinking_budget"), merged.get("max_tokens")
    if _positive_int(budget):
        if budget < THINKING_FLOOR:
            return f"thinking_budget={budget} is below the API's {THINKING_FLOOR}"
        if _positive_int(mx) and budget >= mx:
            return (
                f"thinking_budget={budget} is not below max_tokens={mx} — the API "
                "refuses it; keep it below max_tokens"
            )
    endpoint = str(merged.get("endpoint") or "").strip()
    left = server.missing(merged)
    if endpoint and left and server.models_url(endpoint) is None:
        return (
            f"{left[0]} is left to the API, but the endpoint {endpoint!r} does not end "
            f"in /messages, so the model list is not known — set {', '.join(left)}"
        )
    return validate(RECORD_SCHEMA, NAME, meta, record)


def _required_int(rec: dict, key: str) -> int:
    v = rec.get(key)
    if not _positive_int(v):
        raise ValueError(
            f"{NAME}: `{key}` is not on the agent record, a positive int, and was not "
            f"read from the API (update_agent <id> {key}=…)"
        )
    return v


def make_provider(id, kernel):
    """Build an AnthropicProvider from the agent record."""
    from anthropic_backend.provider import AnthropicProvider

    rec = kernel.get(id) or {}
    t = rec.get("temperature")
    return AnthropicProvider(
        endpoint=required_meta(rec, "endpoint", NAME),
        model=required_meta(rec, "model", NAME),
        key=key_reader(id, kernel, NAME, required=False),
        max_tokens=_required_int(rec, "max_tokens"),
        temperature=float(t) if t is not None else None,
        thinking_budget=rec.get("thinking_budget") or None,
        prompt_cache=bool(rec.get("prompt_cache")),
        usage=usage_of(id),
    )


def _usage_now(id, kernel) -> dict:
    """The agent's counts, with the record's prices applied."""
    rec = kernel.get(id) or {}
    return usage_of(id).snapshot(
        rec.get("price_input_per_mtok"), rec.get("price_output_per_mtok")
    )


async def _reflect_extra(id, kernel):
    """Merged into reflect: what the next request will send, and the usage so far."""
    rec = kernel.get(id) or {}
    return {
        "max_tokens": rec.get("max_tokens"),
        "temperature": rec.get("temperature"),
        "thinking_budget": rec.get("thinking_budget"),
        "prompt_cache": bool(rec.get("prompt_cache")),
        "usage": _usage_now(id, kernel),
    }


async def _usage(id, payload, kernel):
    """usage — args: reset? (bool). The agent's token usage since its kernel started:
    requests, input, cache write and read, output, the derived prompt_tokens, hit_rate
    and saved_tokens (cost_usd and saved_usd when the record has both prices). reset:
    true returns the counts, then zeroes them."""
    reset = payload.get("reset", False)
    if not isinstance(reset, bool):
        return refusal(BAD_ARG, "usage: reset must be true or false", arg="reset")
    out = _usage_now(id, kernel)
    if reset:
        usage_of(id).reset()
    return out


async def on_delete(agent) -> None:
    """The shared teardown, and this agent's counts dropped with it."""
    _USAGE.pop(agent.id, None)
    await _core_on_delete(agent)


def _build():
    verbs, core = build(
        sentence="Anthropic Messages-API LLM agent (raw prompt-and-parse tool-calling).",
        make_provider=make_provider,
        name=NAME,
        extra_verbs={"usage": _usage},
        reflect_extra=_reflect_extra,
    )

    async def handler(id: str, payload: dict, kernel) -> dict | None:
        # A verb that runs a turn first reads what the record leaves to the API; an
        # API that does not answer refuses the verb, naming the field.
        if payload.get("type") in server.TURN_VERBS:
            refused = await server.resolve(
                id,
                kernel,
                key=key_reader(id, kernel, NAME, required=False),
                validate=validate_meta,
            )
            if refused is not None:
                return refused
        return await core(id, payload, kernel)

    return verbs, handler


VERBS, handler = _build()

# Served by the kernel's `docs` verb (tier detailed) — shared by every backend.
from ai_core import CANCEL, EMITS, NOTES  # noqa: E402, F401
