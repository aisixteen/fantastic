"""vllm bundle — a context-managed LLM completion agent talking DIRECTLY to vLLM.

A THIN binding over `ai_core`, like every backend: this module supplies the
`VllmProvider` builder, the write-time checks and the backend's identity, and
`ai_core.build()` returns the `(VERBS, handler)` bound to it. There is NO
agentic loop — one provider pass per send.

Speaks the OpenAI chat wire, and is named for vLLM on purpose: reflect must
describe the real server, and vLLM has its own knobs (`top_k`,
`reasoning_effort`, `thinking_token_budget`, `no_repeat_ngram_size`), its own
failure shapes and — the reason this bundle
exists — a KV-cache session table (`kv_slots` + `kv_overflow`) shared per
endpoint. See `provider.py` for what each is and why. What the record leaves out
— `model`, `max_tokens`, `context_window`, `kv_slots` — is read from the server
before a turn and written back (`server.py`).

`on_cancel` is re-exported from `ai_core` because that module-level name IS the
bundle's cancel hook; `on_delete` is ai_core's, plus this backend's own table.
"""

from __future__ import annotations

from ai_core import build, on_cancel, required_meta  # noqa: F401 — the cancel hook
from ai_core import caching
from ai_core import on_delete as _core_on_delete
from ai_core.credentials import key_reader
from ai_core.schema import credentials_field, field, schema, validate
from kernel._refusal import bad

from vllm_backend import server
from vllm_backend.provider import (
    CONNECT_TIMEOUT,
    DIALECTS,
    KV_OVERFLOWS,
    KV_TTL,
    PASSTHROUGH,
    READ_TIMEOUT,
    REASONING_EFFORTS,
    SAMPLING_DEFAULTS,
    _usage_of,
    last_budgets,
    table_for,
    usage,
)

NAME = "vllm_backend"


def _required_int(rec: dict, key: str) -> int:
    """A count the turn runs on — set on the record, or read from the server
    before the turn (`server.resolve`); fails the turn loud otherwise."""
    v = rec.get(key)
    if isinstance(v, bool) or not isinstance(v, int) or v < 1:
        raise ValueError(
            f"{NAME}: `{key}` is not on the agent record, a positive int, and was not "
            f"read from the server (update_agent <id> {key}=…)"
        )
    return v


def _sampling(rec: dict) -> tuple[dict, bool]:
    """The effective sampling — the record's values over the defaults — and
    whether `thinking_token_budget` was set on the RECORD (explicit: sent as
    is) or defaulted (ours to adapt per call)."""
    eff = {
        k: rec[k] if rec.get(k) is not None else v for k, v in SAMPLING_DEFAULTS.items()
    }
    for k in PASSTHROUGH:
        if rec.get(k) is not None:
            eff[k] = rec[k]
    return eff, rec.get("thinking_token_budget") is not None


def make_provider(id, kernel):
    """Build a VllmProvider from the agent record. `endpoint` and `kv_overflow`
    are the record's; `model`, `max_tokens`, `context_window` and `kv_slots` are
    the record's or were read from the server before the turn."""
    from vllm_backend.provider import VllmProvider

    rec = kernel.get(id) or {}
    sampling, explicit = _sampling(rec)
    overflow = rec.get("kv_overflow")
    if overflow not in KV_OVERFLOWS:
        raise ValueError(
            f"{NAME}: `kv_overflow` is required on the agent record — "
            "evict (the least-recently-used conversation rotates out) or reject "
            "(refuse the newcomer) (update_agent <id> kv_overflow=…)"
        )
    return VllmProvider(
        agent_id=id,
        endpoint=required_meta(rec, "endpoint", NAME),
        model=required_meta(rec, "model", NAME),
        key=key_reader(id, kernel, NAME, required=False),
        max_tokens=_required_int(rec, "max_tokens"),
        window=_required_int(rec, "context_window"),
        kv_slots=_required_int(rec, "kv_slots"),
        kv_overflow=overflow,
        kv_ttl=float(rec.get("kv_ttl") or KV_TTL),
        dialect=rec.get("server_dialect") or "vllm",
        sampling=sampling,
        explicit_budget=explicit,
        connect_timeout=float(rec.get("connect_timeout") or CONNECT_TIMEOUT),
        read_timeout=float(rec.get("read_timeout") or READ_TIMEOUT),
    )


async def _reflect_extra(id, kernel):
    """Merged into reflect: the EFFECTIVE sampling (defaults filled in), the
    timeouts, and the endpoint's shared KV session table — so one reflect shows
    what the next request will send and how many conversations are live."""
    rec = kernel.get(id) or {}
    sampling, explicit = _sampling(rec)
    endpoint = rec.get("endpoint")
    return {
        "sampling": sampling,
        "thinking_token_budget_source": "record" if explicit else "default",
        # what the last pass actually SENT (the default adapts to small calls)
        "last_thinking_budget": last_budgets.get(id),
        "max_tokens": rec.get("max_tokens"),
        "timeouts": {
            "connect": float(rec.get("connect_timeout") or CONNECT_TIMEOUT),
            "read": float(rec.get("read_timeout") or READ_TIMEOUT),
        },
        "kv": {
            "slots": rec.get("kv_slots"),
            "overflow": rec.get("kv_overflow"),
            "ttl": float(rec.get("kv_ttl") or KV_TTL),
            # shared by every vllm_backend agent on the same endpoint
            **(table_for(endpoint).view() if isinstance(endpoint, str) else {}),
        },
        "server_dialect": rec.get("server_dialect") or "vllm",
        "usage": dict(_usage_of(id)),
    }


#: THE RECORD SCHEMA — what this kind's record takes, read with
#: no agent existing (`reflect bundles=all`). Built from the constants the provider
#: runs on, and enforced by `validate_meta` below, so the page, the write check and
#: the turn cannot disagree. `endpoint` and `kv_overflow` are required; the counts
#: a record leaves out are READ FROM THE SERVER before a turn (`server.py`), never
#: given a default.
_FROM_SERVER = "; left out, read from the server before a turn and written here"
RECORD_SCHEMA = schema(
    [
        field("endpoint", "str", "the vLLM server's /v1 URL", required=True),
        field(
            "model",
            "str",
            "the served model name" + _FROM_SERVER + " (the one model it serves)",
        ),
        field(
            "max_tokens",
            "int",
            "a turn's output budget (tokens)" + _FROM_SERVER + " (max_model_len)",
        ),
        field(
            "context_window",
            "int",
            "the server's window (tokens)" + _FROM_SERVER + " (max_model_len)",
        ),
        field(
            "kv_slots",
            "int",
            "how many conversations may be live on the endpoint"
            + _FROM_SERVER
            + " (floor of kv_cache_max_concurrency on metrics_url)",
        ),
        field(
            "metrics_url",
            "str",
            "the server's Prometheus page, read for kv_slots; left out, the "
            "endpoint with /v1 replaced by /metrics",
        ),
        field(
            "kv_overflow",
            "enum",
            "all slots live: evict (the least-recently-used rotates out) or reject "
            "(the newcomer is refused)",
            required=True,
            enum=KV_OVERFLOWS,
        ),
        field(
            "server_dialect",
            "enum",
            "how this server names things: vllm (default) sends thinking_token_budget; "
            "strata sends reasoning_budget_tokens; llamacpp takes no per-request budget "
            "(a server flag) — set explicitly, never guessed (#207)",
            enum=DIALECTS,
        ),
        field(
            "temperature",
            "float",
            "sampling temperature",
            default=SAMPLING_DEFAULTS["temperature"],
        ),
        field("top_p", "float", "nucleus sampling", default=SAMPLING_DEFAULTS["top_p"]),
        field("top_k", "int", "top-k sampling", default=SAMPLING_DEFAULTS["top_k"]),
        field(
            "reasoning_effort",
            "enum",
            "how much the model wants to think (xhigh needs a thinking_token_budget)",
            enum=REASONING_EFFORTS,
            default=SAMPLING_DEFAULTS["reasoning_effort"],
        ),
        field(
            "thinking_token_budget",
            "int",
            "the hard cap on thinking tokens, below max_tokens; unset, a turn sends "
            "min(default, max_tokens/2)",
            default=SAMPLING_DEFAULTS["thinking_token_budget"],
        ),
        field("min_p", "float", "min-p sampling; sent only when set"),
        field("presence_penalty", "float", "sent only when set"),
        field("frequency_penalty", "float", "sent only when set"),
        field("repetition_penalty", "float", "sent only when set"),
        field("no_repeat_ngram_size", "int", "sent only when set"),
        field("seed", "int", "sent only when set"),
        field("stop", "list", "stop sequences; sent only when set"),
        field(
            "kv_ttl",
            "float",
            "seconds an idle conversation keeps its slot",
            default=KV_TTL,
        ),
        field(
            "connect_timeout",
            "float",
            "seconds to reach the server",
            default=CONNECT_TIMEOUT,
        ),
        field(
            "read_timeout",
            "float",
            "seconds to wait on a generation",
            default=READ_TIMEOUT,
        ),
        # the door's key, when it wants one — absent = no Authorization header
        credentials_field(required=False),
    ],
)

#: The fields a turn cannot run without — refused at the write when absent. The
#: schema's required fields, not a second list.
REQUIRED = tuple(f["name"] for f in RECORD_SCHEMA["fields"] if f["required"])


def _positive(v, *, integer: bool) -> bool:
    if isinstance(v, bool):
        return False
    if integer:
        return isinstance(v, int) and v >= 1
    return isinstance(v, (int, float)) and v > 0


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse a write this backend cannot honour, AT THE WRITE, naming the legal
    set. Checks the MERGED state, because `thinking_token_budget` and
    `max_tokens` are only wrong together. Then ai_core's caching refusal: this
    backend cannot do provider caching."""
    merged = {**record, **meta}
    for key in ("max_tokens", "context_window", "kv_slots"):
        if (
            key in meta
            and meta[key] is not None
            and not _positive(meta[key], integer=True)
        ):
            return f"{key} must be a positive int"
    for key in ("kv_ttl", "connect_timeout", "read_timeout"):
        if (
            key in meta
            and meta[key] is not None
            and not _positive(meta[key], integer=False)
        ):
            return f"{key} must be a positive number of seconds"
    url = meta.get("metrics_url")
    if url is not None and not (
        isinstance(url, str) and url.strip().startswith(("http://", "https://"))
    ):
        return "metrics_url must be an http(s) URL — the server's /metrics page"
    if (
        merged.get("kv_slots") is None
        and str(merged.get("endpoint") or "").strip()
        and server.metrics_url(merged) is None
    ):
        return (
            f"kv_slots is left to the server, but the endpoint "
            f"{merged['endpoint']!r} does not end in /v1, so its metrics page is not "
            "known — set metrics_url (the server's /metrics page) or kv_slots"
        )
    if meta.get("kv_overflow") is not None and meta["kv_overflow"] not in KV_OVERFLOWS:
        return (
            f"kv_overflow={meta['kv_overflow']!r} is not a rotation policy — legal "
            "values: evict (the least-recently-used conversation rotates out), "
            "reject (refuse the newcomer)"
        )
    if (
        meta.get("reasoning_effort") is not None
        and meta["reasoning_effort"] not in REASONING_EFFORTS
    ):
        return (
            f"reasoning_effort={meta['reasoning_effort']!r} is not one vLLM accepts "
            '— legal values: low, medium, xhigh ("high" answers 400; xhigh needs '
            "a thinking_token_budget or it can think without answering)"
        )
    budget = merged.get("thinking_token_budget")
    if "thinking_token_budget" in meta and budget is not None:
        if not _positive(budget, integer=True):
            return "thinking_token_budget must be a positive int"
    mx = merged.get("max_tokens")
    if (
        budget is not None
        and _positive(budget, integer=True)
        and _positive(mx, integer=True)
        and budget >= mx
    ):
        return (
            f"thinking_token_budget={budget} is not below max_tokens={mx} — it could "
            "never fire, and a turn would end mid-thought with no answer; keep it "
            "below max_tokens (half is a good start)"
        )
    if "server_dialect" in meta and meta["server_dialect"] is not None:
        if meta["server_dialect"] not in DIALECTS:
            return "server_dialect must be one of vllm, strata, llamacpp"
    if merged.get("server_dialect") == "llamacpp" and budget is not None:
        return (
            "llama.cpp takes the reasoning budget as a server flag, not per request — "
            "unset thinking_token_budget (or use server_dialect strata/vllm)"
        )
    # EVERY required field, checked at the WRITE: a model spawning a sub-AI copies
    # the fields it can see, and a record the write accepted but no turn could use
    # would fail only later, on every turn, far from the create that caused it.
    # Named ALL at once, from the schema, so one retry fixes the record.
    return validate(RECORD_SCHEMA, NAME, meta, record) or caching.refusal(
        meta, record, NAME, False
    )


async def on_delete(agent) -> None:
    """The bundle's delete hook: ai_core's (every row it keeps for the id), then
    this backend's own — the agent's last-sent thinking budget."""
    await _core_on_delete(agent)
    last_budgets.pop(agent.id, None)
    usage.pop(agent.id, None)


async def _usage(id, payload, kernel):
    """usage — args: reset? (bool). The agent's token usage since its kernel started: requests, prompt_tokens, output_tokens. reset: true returns the counts, then zeroes them."""
    reset = payload.get("reset", False)
    if not isinstance(reset, bool):
        return bad("usage", "reset", "true or false")
    u = _usage_of(id)
    counts = dict(u)
    if reset:
        u.update({"requests": 0, "prompt_tokens": 0, "output_tokens": 0})
    return counts


def _build():
    verbs, core = build(
        sentence=(
            "vLLM-backed LLM agent, talking to the vLLM server directly "
            "(raw prompt-and-parse tool-calling; a KV-cache session table per endpoint)."
        ),
        make_provider=make_provider,
        name=NAME,
        reflect_extra=_reflect_extra,
        extra_verbs={"usage": _usage},
    )

    async def handler(id: str, payload: dict, kernel) -> dict | None:
        # A verb that runs a turn first reads what the record leaves to the
        # server; a server that does not answer refuses the verb, naming the field.
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
