"""`context_strategy: none` — never shrink (#257, Rust #292). An over-budget turn is
REFUSED before any model call or row write; nothing is dropped, folded or compacted.
Deterministic, no network. Helpers mirror test_context.py."""

from __future__ import annotations

import pytest

from ai_core.context import estimate_tokens
from ai_core.core import _project_context
from ai_core.strategies import get_strategy, strategy_names

from _fake import FakeCtx  # noqa: E402
from test_context import _FakeProvider, _cfg, _msgs, _turn  # noqa: E402


class _CountingProvider:
    """Records every provider call; the refused turn must make none."""

    def __init__(self):
        self.calls = []

    async def chat(self, messages, max_tokens=None):
        self.calls.append(("chat", messages))
        yield "SHOULD-NOT-BE-CALLED"

    async def summarize(self, messages, max_tokens=None):
        self.calls.append(("summarize", messages))
        return "SHOULD-NOT-BE-CALLED"


def _none_rec(window: int = 400) -> dict:
    return {
        "context_window": window,
        "output_reserve": 0,
        "context_strategy": "none",
    }


# ── registry ────────────────────────────────────────────────────────


def test_none_is_a_known_strategy():
    assert get_strategy("none") is not None
    assert "none" in strategy_names()
    assert strategy_names() == ["compact", "truncate", "strip", "fold", "none"]


def test_an_unknown_strategy_is_still_refused():
    assert get_strategy("shrink") is None
    assert get_strategy("") is None


# ── the seam: refusal ───────────────────────────────────────────────


async def test_an_over_budget_turn_is_refused_under_none():
    msgs = _msgs(40)
    out = await _project_context(
        _none_rec(),
        _cfg(),
        _FakeProvider(),
        "ai",
        FakeCtx(),
        "cli",
        msgs,
        len(msgs) - 1,
    )
    assert isinstance(out, dict), out
    assert out["error"].startswith("test_backend: context full:")
    assert out["error"].endswith("clear, fold by hand, or raise context_window")
    assert "unknown context_strategy" not in out["error"]


async def test_a_refused_none_turn_drops_nothing_and_never_calls_the_provider():
    msgs = _msgs(40)
    before_ids = [id(m) for m in msgs]
    before_len = len(msgs)
    provider = _CountingProvider()
    k = FakeCtx()
    out = await _project_context(
        _none_rec(), _cfg(), provider, "ai", k, "cli", msgs, len(msgs) - 1
    )
    assert isinstance(out, dict) and "context full" in out["error"]
    assert provider.calls == []
    assert k.rows == {}
    assert k.store_calls == []
    assert [p for _t, p in k.sent if p.get("type") == "context"] == []
    assert len(msgs) == before_len
    assert [id(m) for m in msgs] == before_ids


async def test_none_passes_a_turn_that_fits_untouched():
    msgs = [_turn("system", "sys"), _turn("user", "hello"), _turn("assistant", "hi")]
    out = await _project_context(
        _none_rec(window=100_000),
        _cfg(),
        _FakeProvider(),
        "ai",
        FakeCtx(),
        "cli",
        msgs,
        len(msgs) - 1,
    )
    assert out == msgs


async def test_none_refuses_only_past_the_trigger_not_before():
    # Under the trigger the seam returns early, so `none` never refuses a turn
    # that would have passed under any other strategy.
    msgs = _msgs(4)
    window = estimate_tokens(msgs) * 10
    out = await _project_context(
        _none_rec(window=window),
        _cfg(),
        _FakeProvider(),
        "ai",
        FakeCtx(),
        "cli",
        msgs,
        len(msgs) - 1,
    )
    assert out == msgs


# ── schema / validation ─────────────────────────────────────────────


@pytest.mark.parametrize(
    "backend",
    ["ollama_backend", "responses_backend", "vllm_backend", "anthropic_backend"],
)
def test_context_strategy_none_is_accepted_by_validation(backend):
    import importlib

    tools = importlib.import_module(f"{backend}.tools")
    # `none` clears the enum; any error left is about the other (missing) fields.
    err = tools.validate_meta({"context_strategy": "none"}, {})
    assert err is None or "context_strategy" not in err
    err = tools.validate_meta({"context_strategy": 3}, {})
    assert err is not None and err.startswith("context_strategy=3 is not one of")
    assert err.endswith("compact, truncate, strip, fold, none")


def test_the_schema_description_names_never_shrink():
    from ai_core.schema import AI_FIELDS

    field = next(f for f in AI_FIELDS if f["name"] == "context_strategy")
    assert "none = never shrink" in field["hint"]
    assert field["enum"] == ["compact", "truncate", "strip", "fold", "none"]
