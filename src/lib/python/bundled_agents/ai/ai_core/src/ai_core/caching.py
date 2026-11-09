"""How the conversation is kept in the model's view: three modes.

none — every stored row, in order, never trimmed (append-only).
cleanup — trim superseded reads, repeated reply bodies and used docs from the view.
provider_cache — none's history plus the provider's cache markers (the end of the
primer row named by cache_primer_until, the end of the last fold, and the TAIL: the
row before the first row that will fold, else the last row).

cleanup and provider_cache also AUTOFOLD the view: a thought, a cut answer and a large
or recalled reply show as an anchor, never stored.
"""

from __future__ import annotations

from ai_core.autofold import will_fold

#: The legal values of `caching`, in the order the schema lists them.
MODES: tuple[str, ...] = ("none", "cleanup", "provider_cache")
#: The mode a record without `caching` runs in.
DEFAULT_MODE = "cleanup"

#: The `caching` field's hint (verbatim from the Rust).
CACHING_HINT = (
    "how the conversation is kept in the model's view: none = every stored row, in "
    "order, never trimmed (append-only); cleanup = trim superseded reads, repeated "
    "reply bodies and used docs from the view; provider_cache = none's history plus "
    "marks, with autofold (the provider's cache markers: the end of the primer row "
    "named by cache_primer_until, the end of the last fold, and the end of the row "
    "before the first row that will fold); provider_cache is refused by "
    "a backend that cannot cache. A switch keeps the view already written: a row a "
    "cleanup left out of it stays out until a context_clear starts a new thread"
)

#: The `cache_primer_until` field's hint (verbatim from the Rust).
CACHE_PRIMER_HINT = (
    "a row id: the row that ends the stable prefix. The primer and every row before "
    "it are pinned — context_fold will not fold a range that starts at or before it, "
    "in any mode. The provider is told where the prefix ends (the end of that row's "
    "text) only under caching=provider_cache; absent, nothing is pinned or marked"
)


def cleans(mode: str) -> bool:
    """True when the view is trimmed (caching=cleanup)."""
    return mode == "cleanup"


def marks(mode: str) -> bool:
    """True when the provider is asked for its cache markers (caching=provider_cache)."""
    return mode == "provider_cache"


def folds(mode: str) -> bool:
    """True when the view is autofolded: anchors for thoughts, cuts and large replies."""
    return mode != "none"


def mode_of(meta: dict) -> str:
    """The record's caching mode; absent means DEFAULT_MODE. Anything else raises."""
    raw = meta.get("caching")
    if raw is None:
        return DEFAULT_MODE
    if isinstance(raw, str) and raw in MODES:
        return raw
    raise ValueError(
        f"caching={raw!r} is not one of the legal values: {', '.join(MODES)}"
    )


def primer_of(meta: dict) -> int | None:
    """The record's cache_primer_until row id, or None when absent. Bad values raise."""
    v = meta.get("cache_primer_until")
    if v is None:
        return None
    if isinstance(v, int) and not isinstance(v, bool) and v > 0:
        return v
    raise ValueError(f"cache_primer_until={v!r} must be a row id (a positive int)")


def settings(meta: dict) -> tuple[str, int | None]:
    """(mode, primer) of a record; raises what mode_of and primer_of raise."""
    return mode_of(meta), primer_of(meta)


def refusal(meta: dict, record: dict, backend: str, provider_cache: bool) -> str | None:
    """Why a write or a turn cannot run as asked, or None."""
    merged = {**record, **meta}
    try:
        mode = mode_of(merged)
    except ValueError:
        mode = None  # the schema's enum check owns that refusal
    if mode == "provider_cache" and not provider_cache:
        return (
            f"{backend} cannot do provider caching: caching=provider_cache is "
            "refused — use none or cleanup"
        )
    if "cache_primer_until" in meta:
        try:
            primer_of(merged)
        except ValueError as e:
            return str(e)
    return None


def unsupported(name: str) -> ValueError:
    """The error for a provider_cache turn on a backend that cannot cache (returned)."""
    return ValueError(
        f"{name}: caching=provider_cache is not supported by this backend — "
        "use none or cleanup"
    )


def mark(messages: list[dict], primer: int | None) -> list[dict]:
    """A NEW list with the primer's end, the last fold's end and the tail flagged. The
    tail (`cache_tail`) is the agent's own mark: the message before the first user
    message after the last assistant one that will fold, else the last message; never the
    system message. Never mutates a message: the messages can be the cached store rows
    themselves."""
    out = list(messages)
    if primer is not None:
        for i, m in enumerate(out):
            if m.get("id") == primer:
                out[i] = {**m, "cache_primer": True}
                break
    for i in range(len(out) - 1, -1, -1):
        if out[i].get("kind") in ("fold", "clear"):
            out[i] = {**out[i], "cache_fold": True}
            break
    last = max((i for i, m in enumerate(out) if m.get("role") == "assistant"), default=-1)
    pending = next(
        (
            i
            for i in range(last + 1, len(out))
            if out[i].get("role") == "user"
            and will_fold(str(out[i].get("content") or ""))
        ),
        None,
    )
    tail = len(out) - 1 if pending is None else pending - 1
    if tail >= 0 and out[tail].get("role") != "system":
        out[tail] = {**out[tail], "cache_tail": True}
    return out
