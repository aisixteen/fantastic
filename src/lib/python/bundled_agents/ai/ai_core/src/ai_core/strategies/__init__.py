"""Context-overflow strategy registry. One strategy per agent (the `context_strategy`
meta on the record), default `compact`. Selection is STATIC config — there is no
runtime try-X-else-Y chain (NO-FALLBACKS). Each strategy lives in its own file."""

from __future__ import annotations

from ai_core.strategies.base import STRATEGY
from ai_core.strategies.compact import compact
from ai_core.strategies.fold import fold
from ai_core.strategies.strip import strip
from ai_core.strategies.truncate import truncate

# NOTE: `memgpt` was removed. Its only distinct behavior was injecting a memory-pressure
# WARNING turn at compaction; that persist nudge is now UNIVERSAL — carried by the one
# canonical context-notice the seam composes for EVERY strategy (`core._context_notice`).
# A separate strategy for it would be a second mechanism (NO-FALLBACKS).
DEFAULT_STRATEGY = "compact"

_STRATEGIES: dict[str, STRATEGY] = {
    "compact": compact,
    "truncate": truncate,
    # `strip` keeps every turn and drops reply BODIES instead — the ledger at mixed
    # resolution. The model's call titles ride on the stubs, its own prose after each
    # reply survives as the distillation, and the cut is only the second stage.
    "strip": strip,
    # `fold` leaves MARKS: closed chains collapse to an appended fold row (summary +
    # refs) the model can descend into; then strip, then cut. The only strategy that
    # writes, because a mark that does not persist is a prefix that rewrites every turn.
    "fold": fold,
}


def get_strategy(name: str) -> STRATEGY | None:
    """Return the strategy by name, or None for an unknown name — the caller surfaces a
    config error, NOT a silent fall-through to the default."""
    return _STRATEGIES.get(name)


def strategy_names() -> list[str]:
    return list(_STRATEGIES)


__all__ = ["DEFAULT_STRATEGY", "STRATEGY", "get_strategy", "strategy_names"]
