"""Side calls (fold summaries, recall choices) strip reasoning like the main turn —
unstripped, a model-written fold is titled `<think>The user wants…`."""

from __future__ import annotations

from ai_core.core import _compose, _make_summarizer
from ai_core.strategies.fold import _summarize
from ai_core.context import ProjectionCtx


class _P:
    def __init__(self, text):
        self.text = text

    async def chat(self, messages, max_tokens=None):
        for i in range(0, len(self.text), 7):
            yield self.text[i : i + 7]


async def test_the_summarizer_drops_the_think_block():
    s = _make_summarizer(
        _P(
            "<think>The user wants me to summarize.</think>\nRead 8 files; all pangram.\n\nNothing binds."
        )
    )
    assert (
        await s([{"role": "user", "content": "x"}])
        == "Read 8 files; all pangram.\n\nNothing binds."
    )


async def test_compose_drops_the_think_block_even_unterminated():
    assert (
        await _compose(_P('<think>hm</think>{"expand": []}'), "p") == '{"expand": []}'
    )
    assert await _compose(_P("<think>cut off mid-thought"), "p") == ""


def _ctx(summ):
    return ProjectionCtx(
        budget=12000,
        recent_n=4,
        summarize=summ,
        self_id="ai",
        kernel=None,
        client_id="c",
    )


async def test_a_fold_title_is_the_first_non_empty_line_after_stripping():
    async def summ(msgs, max_tokens=None):
        return (
            '\n\n"Read 8 files; all pangram."\n\nAsked to read; found nothing unusual.'
        )

    rows = [
        {"role": "user", "content": "go", "id": "u:1", "parent": None, "title": "t"}
    ]
    title, body, degraded = await _summarize(_ctx(summ), rows, [0])
    assert (title, degraded) == ("Read 8 files; all pangram.", False)
    assert body == "Asked to read; found nothing unusual."


async def test_a_summary_that_still_opens_a_think_block_degrades_to_titles():
    async def summ(msgs, max_tokens=None):
        return "<think>never closed"

    rows = [
        {"role": "user", "content": "go", "id": "u:1", "parent": None, "title": "t"}
    ]
    title, body, degraded = await _summarize(_ctx(summ), rows, [0])
    assert degraded is True and "<think" not in title


def test_thinking_is_every_span_joined():
    from ai_core import core

    assert core.thinking_fields("<think>a\nb</think>answer") == {"thinking": "a\nb"}
    # every span joined by a blank line, an unclosed last span included
    assert core.thinking_fields("<think>x</think>mid<think>y") == {"thinking": "x\n\ny"}
    assert core.thinking_fields("plain") == {}
    long = "<think>" + "z" * (core.THINKING_CAP + 5) + "</think>"
    out = core.thinking_fields(long)
    assert out["thinking_cut"] == 5
    assert len(out["thinking"]) == core.THINKING_CAP
