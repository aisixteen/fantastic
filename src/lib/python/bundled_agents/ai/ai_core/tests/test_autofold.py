"""AUTOFOLD (#556): a thought, a cut answer and a large or recalled reply show as an anchor
in the model's view; the store keeps every row whole; `recall` with an id returns a row
whole; the agent places the cache tail mark. Rust twin: `history_view.rs`,
`caching.rs`, `tests_caching.rs`, `tests_autofold.rs`."""

from __future__ import annotations

from _fake import FakeCtx

from ai_core import call_check, caching, core
from ai_core.autofold import autofold_row, will_fold
from ai_core.constants import AUTOFOLD_AT
from ai_core.core import _recall


def _row(i, role, content, **extra):
    return {"role": role, "id": i, "content": content, **extra}


def _fold(hist, i, incoming=None):
    return autofold_row(hist, i, hist[i], incoming)["content"]


def _big(n_chars):
    return "first line\n" + "x" * (n_chars - 11)


def _correction_for(answer):
    return f"[reply to - · - - · ↑9]\n{call_check.check(answer, ran=0)[0]}"


def _send(payload):
    from ai_core.tool_parse import render_call

    return render_call("send", {"target_id": "ai", "payload": payload})


# ── pure ────────────────────────────────────────────────────────────────────


def test_a_row_with_thinking_shows_its_thought_anchor():
    hist = [_row(5, "assistant", "answer", thinking="t" * 400)]
    shown = _fold(hist, 0)
    assert shown == "[thought ~100 tokens · id 5]\nanswer"
    assert "tttt" not in shown
    assert _fold([_row(6, "assistant", "answer")], 0) == "answer"
    assert (
        _fold([_row(7, "assistant", "", thinking="t" * 8)], 0)
        == "[thought ~2 tokens · id 7]"
    )


def test_the_thought_anchor_counts_the_cut_thinking_too():
    hist = [_row(5, "assistant", "answer", thinking="t" * 400, thinking_cut=400)]
    assert _fold(hist, 0) == "[thought ~200 tokens · id 5]\nanswer"


def test_a_reply_body_over_autofold_at_folds_once_answered():
    tag = "[reply to 3 · notes read]\n"
    over = tag + _big(AUTOFOLD_AT * 4 + 4)
    hist = [_row(8, "user", over), _row(9, "assistant", "ok")]
    assert _fold(hist, 0) == (
        '[folded · read reply from notes · ~16001 tokens · head: "first line" · get: 8]'
    )
    at = tag + _big(AUTOFOLD_AT * 4)
    assert _fold([_row(8, "user", at), _row(9, "assistant", "ok")], 0) == at
    assert _fold([_row(8, "user", over)], 0) == over  # unanswered: whole


def test_a_batch_folds_only_its_large_block():
    small = "[reply to 3 · notes list · 1 of 2 in batch b]\nsmall body"
    large = "[reply to 3 · notes read · 2 of 2 in batch b]\n" + _big(AUTOFOLD_AT * 4 + 8)
    hist = [_row(8, "user", f"{small}\n\n{large}"), _row(9, "assistant", "ok")]
    first, second = _fold(hist, 0).split("\n\n")
    assert first == small
    assert second.startswith("[folded · read reply from notes · ~")
    assert second.endswith(" · get: 8]")


def test_an_answered_recall_reply_folds_whatever_its_size_and_names_what_it_recalled():
    hist = [
        _row(20, "assistant", _send({"type": "recall", "id": 12})),
        _row(21, "user", "[reply to 7 · ai recall · ↑20]\ntiny"),
        _row(22, "assistant", "ok"),
        _row(23, "assistant", _send({"type": "recall", "query": "codename"})),
        _row(24, "user", "[reply to 7 · ai recall · ↑23]\ntiny"),
        _row(25, "assistant", "ok"),
        _row(26, "user", "[reply to 7 · ai recall]\ntiny"),
        _row(27, "assistant", "ok"),
    ]
    assert _fold(hist, 1) == (
        "[folded · recall reply from ai · id 12 · ~1 tokens · get: 21]"
    )
    assert _fold(hist, 4) == (
        '[folded · recall reply from ai · query "codename" · ~1 tokens · get: 24]'
    )
    assert _fold(hist, 6) == "[folded · recall reply from ai · ~1 tokens · get: 26]"
    assert _fold(hist[:2], 1) == "[reply to 7 · ai recall · ↑20]\ntiny"


def test_a_cut_row_folds_at_once_with_its_action():
    answer = 'Sending.\n<invoke name="send">'
    fix = _correction_for(answer)
    for hist, incoming in (
        ([_row(30, "assistant", answer, thinking="t" * 28)], fix),
        ([_row(30, "assistant", answer, thinking="t" * 28), _row(31, "user", fix)], None),
    ):
        shown = _fold(hist, 0, incoming)
        assert shown.startswith(
            'Sending.\n[cut · you wrote <invoke name="send"> after thinking ~7 tokens'
        ), shown
        assert " · nothing ran · ACTION: " in shown, shown
        assert shown.endswith(" · full: 30]"), shown
    one = [_row(30, "assistant", '<invoke name="send">')]
    assert _fold(one, 0, fix).startswith(
        '[cut · you wrote <invoke name="send"> · nothing ran'
    )


def test_an_unsupported_tag_correction_gives_the_caret_instruction_as_action():
    # the Rust checker's report for a tag outside the call grammar (the Python
    # checker does not write this one yet; the fold reads the text either way)
    fix = (
        "[reply to - · - - · ↑9]\n[unsupported tag — not read] line 1, col 7:\n"
        "  Hello <b>\n"
        "        ^^^ `<b>` is not part of the call grammar — delete it\n"
        "Write a call as: <call name=\"send\">…</call>"
    )
    hist = [_row(30, "assistant", "Hello <b>", thinking="t" * 4)]
    assert (
        "ACTION: `<b>` is not part of the call grammar — delete it · full: 30]"
        in _fold(hist, 0, fix)
    )


def test_a_row_before_an_ordinary_reply_or_a_not_dispatched_report_is_not_a_cut():
    a = _row(30, "assistant", 'text\n<call name="done"></call>', thinking="t" * 4)
    ordinary = "[reply to 5 · notes read]\nbody"
    not_run = "[reply to - · - -]\ncall NOT dispatched: a send names its target"
    for nxt in (ordinary, not_run, "plain user text"):
        assert _fold([a, _row(31, "user", nxt)], 0) == (
            '[thought ~1 tokens · id 30]\ntext\n<call name="done"></call>'
        )
    assert _fold([a], 0, not_run).startswith("[thought ~")


def test_will_fold_is_true_for_a_recall_or_a_large_block_only():
    assert will_fold("[reply to 3 · ai recall]\nx")
    assert will_fold("[reply to 3 · notes read]\n" + "x" * (AUTOFOLD_AT * 4 + 4))
    assert not will_fold("[reply to 3 · notes read]\n" + "x" * (AUTOFOLD_AT * 4))
    assert not will_fold("[reply to 3 · notes read]\nsmall")
    assert not will_fold("x" * (AUTOFOLD_AT * 8))


# ── mark ────────────────────────────────────────────────────────────────────


def _big_reply():
    return "[reply to 3 · notes read]\n" + "x" * (AUTOFOLD_AT * 4 + 4)


def _tails(msgs):
    return [i for i, m in enumerate(msgs) if m.get("cache_tail")]


def test_folds_runs_in_cleanup_and_provider_cache_only():
    assert caching.folds("cleanup") and caching.folds("provider_cache")
    assert not caching.folds("none")


def test_mark_puts_the_tail_mark_on_the_last_message_when_nothing_will_fold():
    msgs = [
        {"role": "system", "content": "s"},
        {"role": "user", "content": "a", "id": 1},
        {"role": "assistant", "content": "b", "id": 2},
        {"role": "user", "content": "[reply to 3 · notes read]\nsmall"},
    ]
    assert _tails(caching.mark(msgs, None)) == [3]
    assert not any("cache_tail" in m for m in msgs), "never mutates"


def test_mark_puts_the_tail_mark_before_the_first_row_that_will_fold():
    msgs = [
        {"role": "system", "content": "s"},
        {"role": "user", "content": "a"},
        {"role": "assistant", "content": "b"},
        {"role": "user", "content": _big_reply()},
    ]
    out = caching.mark(msgs, None)
    assert _tails(out) == [2] and "cache_tail" not in out[3]
    msgs[3] = {"role": "user", "content": "[reply to 3 · ai recall]\nx"}
    assert _tails(caching.mark(msgs, None)) == [2]
    older = [
        {"role": "system", "content": "s"},
        {"role": "user", "content": _big_reply()},
        {"role": "assistant", "content": "b"},
        {"role": "user", "content": "live"},
    ]
    assert _tails(caching.mark(older, None)) == [3]


def test_the_tail_mark_never_lands_on_the_system_message():
    msgs = [
        {"role": "system", "content": "s"},
        {"role": "user", "content": _big_reply()},
    ]
    assert _tails(caching.mark(msgs, None)) == []


# ── the view, over a store ──────────────────────────────────────────────────


def _k(**rec):
    return FakeCtx({"context_window": 32000, "output_reserve": 0, **rec})


def _put(k, role, content, **extra) -> int:
    return k.rows_put("ai", {"role": role, "content": content, **extra})


async def _view(k, mode):
    return (await core._assemble("ai", "next", k, "c", system_prompt="sys", mode=mode))[
        0
    ]


def _stored(k):
    return [
        (r["role"], r["content"])
        for r in k.rows_for("ai")
        if r.get("role") in ("user", "assistant")
    ]


async def test_autofold_runs_in_cleanup_and_provider_cache_and_none_stays_append_only():
    k = _k()
    _put(k, "user", "hello")
    _put(k, "assistant", "answer", thinking="t" * 40)
    _put(k, "user", "[reply to 12 · notes read]\nhead\n" + "x" * (AUTOFOLD_AT * 4 + 40))
    _put(k, "assistant", "ok")
    before = _stored(k)
    none = await _view(k, "none")
    assert [(m["role"], m["content"]) for m in none[1:-1]] == before
    for mode in ("cleanup", "provider_cache"):
        seen = "\n".join(m["content"] for m in (await _view(k, mode))[1:-1])
        assert "[thought ~" in seen, mode
        assert "[folded · read reply from notes · ~" in seen, mode
        assert "xxxxxxxxxx" not in seen, mode
    assert _stored(k) == before


# ── turns ───────────────────────────────────────────────────────────────────


class _Script:
    def __init__(self, *texts):
        self.texts = list(texts)
        self.calls = []

    async def chat(self, messages, max_tokens=None):
        self.calls.append(messages)
        yield self.texts.pop(0) if self.texts else '<call name="done"></call>'


def _cfg(provider_cache=True):
    return core.Backend(
        sentence="s",
        make_provider=lambda *a: None,
        name="mock_backend",
        provider_cache=provider_cache,
    )


async def test_thinking_folds_to_its_anchor_from_the_next_request(fake_providers):
    k = _k()
    p = _Script('<think>SECRET-PLAN</think>Answer one.\n<call name="done"></call>')
    fake_providers["ai"] = p
    try:
        await core._run("ai", "go", k, "c", _cfg(False))
        await core._run("ai", "again", k, "c", _cfg(False))
    finally:
        fake_providers.pop("ai", None)
    seen = str(p.calls[1])
    assert "[thought ~" in seen and "Answer one." in seen
    assert "SECRET-PLAN" not in seen
    row = next(r for r in k.rows_for("ai") if r.get("role") == "assistant")
    assert row["thinking"] == "SECRET-PLAN"


async def test_a_provider_cache_turn_marks_before_the_large_body_and_the_next_turn_keeps_that_prefix(
    fake_providers,
):
    k = _k(caching="provider_cache", context_window=1_000_000)
    _put(k, "user", "hi")
    _put(k, "assistant", "yo")
    text = "[reply to 5 · notes read]\nhead\n" + "x" * (AUTOFOLD_AT * 4 + 40)
    p = _Script("ok", "ok", "ok", "ok")
    fake_providers["ai"] = p
    try:
        await core._run("ai", text, k, "c", _cfg())
        await core._run("ai", "next", k, "c", _cfg())
    finally:
        fake_providers.pop("ai", None)
    msgs1 = next(c for c in p.calls if c[-1]["content"] == text)
    msgs2 = next(c for c in p.calls if c[-1]["content"] == "next")
    n = len(msgs1)
    assert "cache_tail" not in msgs1[-1]
    assert [i for i, m in enumerate(msgs1) if m.get("cache_tail")] == [n - 2]

    def pair(m):
        return (m["role"], m["content"])

    assert [pair(m) for m in msgs1[: n - 1]] == [pair(m) for m in msgs2[: n - 1]]
    assert msgs2[n - 1]["content"].startswith("[folded · read reply from notes")
    assert msgs2[-1].get("cache_tail") is True


# ── recall by id ────────────────────────────────────────────────────────────


async def test_recall_by_id_returns_the_row_whole_with_its_thinking():
    k = _k()
    rid = _put(k, "assistant", "the answer", thinking="the deep thought")
    for given in (rid, str(rid)):
        r = await _recall("ai", {"type": "recall", "id": given}, k)
        assert r == {
            "messages": [
                {
                    "index": 0,
                    "role": "assistant",
                    "content": "the answer",
                    "thinking": "the deep thought",
                }
            ],
            "total": 1,
            "truncated": False,
        }
    r = await _recall("ai", {"type": "recall", "id": 99999}, k)
    assert r["reason"] == "not_found" and r["arg"] == "id", r
    r = await _recall("ai", {"type": "recall", "id": "x"}, k)
    assert r["reason"] == "bad_arg" and r["arg"] == "id", r

