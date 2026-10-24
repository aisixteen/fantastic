"""`none` — never shrink. The one mode that DROPS NOTHING: `_project_context` refuses an
over-budget turn before this strategy is ever called ("context full: …"), so no model
call is made, no row is written and no turn is folded, compacted or cut. This function
exists so the registry stays uniform (every name maps to a strategy of the STRATEGY
signature) and under budget it returns the body untouched. #257 / Rust #292."""

from __future__ import annotations

from ai_core.context import ProjectionCtx
from ai_core.strategies.base import Projection


async def none(
    body: list[dict], system_block: list[dict], rec: dict, ctx: ProjectionCtx
) -> Projection:
    return Projection(body=list(body))
