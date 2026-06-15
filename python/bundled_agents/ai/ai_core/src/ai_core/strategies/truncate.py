"""`truncate` — keep the first (task-framing) turn + the recent turns, drop the middle.
Cheap: NO summarizer call. The elision is reported via the `Projection.omitted_marker`
flag (NOT a fabricated turn); the seam renders the one canonical context-notice."""

from __future__ import annotations

from ai_core.context import ProjectionCtx, estimate_one
from ai_core.strategies.base import NOTICE_ENVELOPE_RESERVE, Projection, fit_tail


async def truncate(
    body: list[dict], system_block: list[dict], rec: dict, ctx: ProjectionCtx
) -> Projection:
    if len(body) <= 1:
        return Projection(body=fit_tail(body, ctx.budget))
    first = body[0]
    # Reserve room for the notice the seam prepends (no summary in truncate).
    head_cost = estimate_one(first) + NOTICE_ENVELOPE_RESERVE
    tail = fit_tail(body[1:], max(0, ctx.budget - head_cost))
    return Projection(body=[first] + tail, omitted_marker=True)
