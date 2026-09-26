"""Context-window budgeting primitives for the ai_core overflow strategies.

A char-based token ESTIMATE — deliberately tokenizer-agnostic. Exactness is
irrelevant for a fit-to-window budget (we under-fill with an output reserve), and
tiktoken would be the WRONG tokenizer for gemma/nemotron anyway. Plus the
window/budget resolution read off the agent record. Used by `ai_core.strategies`
and the projection seam in `core._run`.
"""

from __future__ import annotations

import json
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Any

CHARS_PER_TOKEN = 4
# 4096 until 2026-08-24, and by then it did not fit the kernel's OWN system block.
# Measured: budget = 4096 - 1024 output reserve = 3072, against a default system
# prompt of ~3174 tok (identity + tree + bundle catalog + the send and context
# howtos). Every agent created without an explicit `context_window` failfasted with
# `too_small` before the model was ever called — the margin had quietly reached zero,
# and a ~100-token addition to a verb doc was enough to take it negative.
#
# 128k is not a guess about any particular model: it is the window a caller gets when
# they say NOTHING, and the honest default is one that fits this kernel's own prompt
# with room to work. A model with a smaller real window is named on the record
# (`context_window`, or ollama's `num_ctx`) — the explicit lever always wins, and the
# compaction path is exercised by whatever value is set, not by this constant.
DEFAULT_CONTEXT_WINDOW = 131072
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
    summarize: Callable[..., Awaitable[str]] | None  # (msgs, max_tokens=None)
    self_id: str
    kernel: Any
    # The conversation this projection is for. `fold` is the one strategy that WRITES
    # (fold rows are marks that must persist); it needs to know which store.
    client_id: str | None = None
