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
    return Projection(body=fit_tail(recent, avail), summary=summary)
