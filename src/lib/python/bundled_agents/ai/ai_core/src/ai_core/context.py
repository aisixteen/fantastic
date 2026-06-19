"""Context-window budgeting primitives for the ai_core overflow strategies.

A char-based token ESTIMATE — deliberately tokenizer-agnostic. Exactness is
irrelevant for a fit-to-window budget (we under-fill with an output reserve), and
tiktoken would be the WRONG tokenizer for gemma/nemotron anyway. Plus the
window/budget resolution read off the agent record. Used by `ai_core.strategies`
and the projection seam in `core._run`.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Awaitable, Callable

CHARS_PER_TOKEN = 4
DEFAULT_CONTEXT_WINDOW = 4096
DEFAULT_OUTPUT_RESERVE = 1024
BUDGET_FLOOR = 256  # never project to a budget below this


def estimate_one(message: dict) -> int:
    """Rough token estimate for ONE message — counts the SERIALIZED form, because the
    role + content + tool_calls + role:tool replies + JSON envelope all consume real
    context (and dominate agentic turns)."""
    n = len(json.dumps(message, default=str))
    return (n + CHARS_PER_TOKEN - 1) // CHARS_PER_TOKEN


def estimate_tokens(messages: list[dict]) -> int:
    return sum(estimate_one(m) for m in messages)


def _as_pos_int(v: Any) -> int | None:
    if isinstance(v, bool):
        return None
    if isinstance(v, int) and v > 0:
        return v
    if isinstance(v, float) and v > 0:
        return int(v)
    if isinstance(v, str) and v.strip().isdigit() and int(v) > 0:
        return int(v)
    return None


def resolve_context_window(rec: dict) -> int:
    """The model's usable window, by STATIC precedence (no fallback-chain):
    `context_window` (the explicit per-agent override — the UNIFORM lever, works on any
    backend incl. nvidia which has no `num_ctx`) → `num_ctx` (ollama's real knob) → a
    conservative default."""
    for key in ("context_window", "num_ctx"):
        v = _as_pos_int(rec.get(key))
        if v is not None:
            return v
    return DEFAULT_CONTEXT_WINDOW


def output_reserve(rec: dict) -> int:
    v = rec.get("output_reserve")
    if isinstance(v, int) and not isinstance(v, bool) and v >= 0:
        return v
    if isinstance(v, str) and v.strip().isdigit():
        return int(v)
    return DEFAULT_OUTPUT_RESERVE


def budget(rec: dict) -> int:
    """Token budget for the INPUT (window minus output headroom), floored."""
    return max(resolve_context_window(rec) - output_reserve(rec), BUDGET_FLOOR)


@dataclass
class ProjectionCtx:
    """Everything a strategy needs, WITHOUT importing `core`.

    `budget` here is the budget for the conversation BODY (the window budget minus
    the already-measured system block) — strategies fit the body to it. `summarize`
    is a closure over the backend provider (None for backends that can't summarize);
    only `compact` uses it.
    """

    budget: int
    recent_n: int
    summarize: Callable[[list[dict]], Awaitable[str]] | None
    self_id: str
    kernel: Any
