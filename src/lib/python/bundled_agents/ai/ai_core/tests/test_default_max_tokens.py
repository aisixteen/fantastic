"""#495 — a record that names no `max_tokens` gets a bounded default, not the model's
maximum: DEFAULT_TURN_TOKENS, plus the thinking budget, never above the model's own."""

from __future__ import annotations

from ai_core.constants import DEFAULT_TURN_TOKENS, default_max_tokens


def test_the_default_is_bounded_grows_with_thinking_and_stops_at_the_models_maximum():
    assert DEFAULT_TURN_TOKENS == 260000
    assert default_max_tokens(None, 64000) == 64000
    assert default_max_tokens(None, 1_000_000) == 260000
    assert default_max_tokens(2048, 1_000_000) == 2048 + 260000
    assert default_max_tokens(None, 8000) == 8000
    assert default_max_tokens(4000, 10000) == 10000
