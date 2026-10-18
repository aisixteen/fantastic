"""Shared helpers + the STRATEGY contract for ai_core context-overflow strategies.

A strategy is an async projection: given the conversation BODY (history + the live user
turn, WITHOUT the system block), return a `Projection` — the shortened body that fits
`ctx.budget`, plus a structured ARTIFACT describing what it did (a summary, or an
omitted-span flag). It does NOT fabricate a notice turn: the single canonical
`[context-notice]` is composed once at the seam (`core._context_notice`) from this
artifact. The system block is re-prepended by the caller (`core._run`). The durable
chat store is NEVER touched — strategies shape only what the model sees this turn.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field

from ai_core.context import ProjectionCtx, estimate_one
from ai_core.constants import NOTICE_ENVELOPE_RESERVE  # noqa: F401 — re-exported seams


@dataclass
class Projection:
    """What a strategy returns: the projected body + the artifact the seam needs to
    compose the canonical context-notice. NEVER a fabricated user turn."""

    body: list[dict]  # projected conversation body — NO notice turn, NO system block
    summary: str | None = None  # compact: the LLM summary text; truncate/none: None
    omitted_marker: bool = False  # truncate: True (an earlier span was elided in place)
    # `halve`: the agent's live FACT BLOCK, rendered. Carried as an ARTIFACT for the
    # same reason `summary` is — a strategy states what it retained, the seam is the one
    # place that turns it into a turn. Empty for every other strategy.
    pinned: list[str] = field(default_factory=list)
    # `strip`: reply blocks reduced to their stub this projection, and the bytes that
    # left the view with them. Reported in the artifact so a probe can prove the
    # mechanism ran, separately from whether the run succeeded.
    stripped: int = 0
    stripped_bytes: int = 0
    folded: int = 0  # `fold`: fold rows written this projection
    unanchored: int = 0  # `fold`: folds left out of the view, their anchor unknown


# (body, system_block, rec, ctx) -> Projection (body excludes the system block)
STRATEGY = Callable[
    [list[dict], list[dict], dict, ProjectionCtx], Awaitable[Projection]
]

STUB_SUMMARY = "[Earlier conversation omitted — summary unavailable]"


def fit_tail(turns: list[dict], budget: int) -> list[dict]:
    """Keep the largest SUFFIX of `turns` that fits `budget`, always including the last
    turn (the live request)."""
    if not turns:
        return []
    kept = [turns[-1]]
    used = estimate_one(turns[-1])
    for t in reversed(turns[:-1]):
        c = estimate_one(t)
        if used + c > budget:
            break
        kept.insert(0, t)
        used += c
    return kept


def recent_split(body: list[dict], recent_n: int) -> tuple[list[dict], list[dict]]:
    """Split `body` into (overflow, recent) at the last `recent_n` turns."""
    start = max(0, len(body) - recent_n)
    return body[:start], body[start:]


async def safe_summary(ctx: ProjectionCtx, overflow: list[dict]) -> str:
    """Summarize the overflow via the backend provider; degrade to a stub on ANY
    failure (a degraded artifact — the full transcript is whole in the chat store)."""
    if ctx.summarize is None:
        return STUB_SUMMARY
    try:
        s = await ctx.summarize(overflow)
    except Exception:
        return STUB_SUMMARY
    return (s or "").strip() or STUB_SUMMARY
