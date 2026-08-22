"""`compact` — the DEFAULT strategy. Keep the recent turns verbatim + an LLM summary of
the overflow. The summary rides the `Projection` artifact (NOT a fabricated turn); the
seam wraps it into the one canonical context-notice."""

from __future__ import annotations

from ai_core.context import ProjectionCtx, estimate_one
from ai_core.strategies.base import (
    NOTICE_ENVELOPE_RESERVE,
    Projection,
    fit_tail,
    recent_split,
    safe_summary,
)


async def compact(
    body: list[dict], system_block: list[dict], rec: dict, ctx: ProjectionCtx
) -> Projection:
    overflow, recent = recent_split(body, ctx.recent_n)
    if not overflow:
        return Projection(body=fit_tail(body, ctx.budget))
    summary = await safe_summary(ctx, overflow)
    # Reserve room for the notice the seam will prepend: the wrapper envelope + the
    # summary text itself (the seam embeds this exact summary). One budget authority.
    summary_cost = estimate_one({"role": "user", "content": summary})
    avail = max(0, ctx.budget - NOTICE_ENVELOPE_RESERVE - summary_cost)
    # PIN THE GOAL. The first turn is the task framing, and it is the OLDEST — so it
    # is the first thing the overflow split takes. A summary is a lossy retelling; the
    # one turn that must survive verbatim is the instruction the whole run serves.
    # Measured 2026-08-20 (live, four runs): the agent reached the correct conclusion,
    # then ended "healthy budget, no tasks" / "Give me a task whenever" — it had not
    # refused to act, it no longer knew what it was doing. `truncate` already pins this
    # turn; `compact` did not. Cost is one turn of budget, charged before the tail.
    first = body[0]
    head_cost = estimate_one(first)
    kept = fit_tail(recent, max(0, avail - head_cost))
    # Only re-add it if the recent window does not already carry it (short bodies).
    if kept and kept[0] is first:
        return Projection(body=kept, summary=summary)
    return Projection(body=[first] + kept, summary=summary)
