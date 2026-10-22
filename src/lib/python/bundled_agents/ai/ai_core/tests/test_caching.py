"""caching modes (none | cleanup | provider_cache), the primer and the last-fold
flags. Port of the Rust #178 task A tests (caching.rs `mod tests` and tests_caching.rs)."""

from __future__ import annotations

import pytest

from ai_core import caching, core, folds
from ai_core.schema import AI_FIELDS, schema, validate
from _fake import FakeCtx


def setup_function(_):
    _reset()


def teardown_function(_):
    _reset()


def _reset():
    for name in ("_calib", "_reported", "_projection", "_compaction_mark"):
        store = getattr(core, name, None)
        if isinstance(store, dict):
            store.pop("ai", None)


# ── helpers ─────────────────────────────────────────────────────────────────


def _cfg(name="mock_backend", provider_cache=False):
    return core.Backend(
        sentence="s",
        make_provider=lambda *a: None,
        name=name,
        provider_cache=provider_cache,
    )


def _k(**rec):
    return FakeCtx({"context_window": 32000, "output_reserve": 0, **rec})


def _put(k, role, content, **extra) -> int:
    return k.rows_put("ai", {"role": role, "content": content, **extra})


def _fold(k, refs, title) -> int:
    fid = k.mint_id()
    k.rows_put(
        "ai",
        folds.make_fold(
            "ai", refs, title, f"body of {title}", False, id=fid, anchor=min(refs)
        ),
    )
    return fid


async def _view(k, mode):
    return (await core._assemble("ai", "next", k, "c", system_prompt="sys", mode=mode))[
        0
    ]


def _history(msgs):
    return [(m["role"], m["content"]) for m in msgs[1:-1]]


def _stored(k):
    return [
        (r["role"], r["content"])
        for r in k.rows_for("ai")
        if r.get("role") in ("user", "assistant")
    ]


def _reply(pid, verb, body):
    return f"[reply to {pid} · notes {verb}]\n{body}"


def _big():
    return "the same long listing, line after line, " * 14


def _docs(pid):
    return (
        f"[reply to {pid} · kernel docs]\n<of>runner</of>\n"
        "<bundle>exec_bridge.tools</bundle>\n<tier>oneliners</tier>\n<verbs>\n"
        "<start>args: command:str</start>\n</verbs>"
    )


class _Seen:
    last_prompt_tokens = None

    def __init__(self):
        self.calls = []

    async def chat(self, messages, max_tokens=None):
        self.calls.append(messages)
        yield 'ok <call name="done"></call>'


async def _turn(k, cfg, fake_providers, text="live"):
    p = _Seen()
    fake_providers["ai"] = p
    try:
        await core._run("ai", text, k, "c", cfg)
    finally:
        fake_providers.pop("ai", None)
    return next(c for c in p.calls if c[-1]["content"] == text)


def _field(sch, name):
    return next(f for f in sch["fields"] if f["name"] == name)


# ── unit tests (port of the `mod tests` in caching.rs) ──────────────────────


def test_the_mode_defaults_to_cleanup_and_takes_the_three_names():
    assert caching.mode_of({}) == "cleanup"
    assert caching.mode_of({"caching": "none"}) == "none"
    assert caching.mode_of({"caching": "cleanup"}) == "cleanup"
    assert caching.mode_of({"caching": "provider_cache"}) == "provider_cache"


def test_an_unknown_mode_is_an_error_that_names_it():
    with pytest.raises(ValueError) as e:
        caching.mode_of({"caching": "always"})
    assert str(e.value) == (
        "caching='always' is not one of the legal values: none, cleanup, provider_cache"
    )
    for bad in (True, ["cleanup"]):
        with pytest.raises(ValueError):
            caching.mode_of({"caching": bad})


def test_only_cleanup_trims_and_only_provider_cache_marks():
    assert [caching.cleans(m) for m in caching.MODES] == [False, True, False]
    assert [caching.marks(m) for m in caching.MODES] == [False, False, True]


def test_the_primer_is_absent_or_a_positive_row_id():
    assert caching.primer_of({}) is None
    assert caching.primer_of({"cache_primer_until": 7}) == 7
    for bad in (0, -3, "7", 2.5, True):
        with pytest.raises(ValueError) as e:
            caching.primer_of({"cache_primer_until": bad})
        msg = str(e.value)
        assert msg.startswith("cache_primer_until=")
        assert msg.endswith("must be a row id (a positive int)")


def test_provider_cache_is_refused_where_the_backend_cannot_cache():
    meta = {"caching": "provider_cache"}
    msg = caching.refusal(meta, {}, "mock_backend", False)
    assert msg is not None and "mock_backend" in msg
    assert "provider caching" in msg
    assert caching.refusal(meta, {}, "mock_backend", True) is None
    assert caching.refusal({"caching": "none"}, {}, "mock_backend", False) is None


def test_a_primer_named_on_the_write_must_be_a_row_id():
    assert caching.refusal({"cache_primer_until": 0}, {}, "b", True) is not None
    assert caching.refusal({"cache_primer_until": 5}, {}, "b", True) is None


def test_mark_flags_the_primer_and_the_last_fold_or_clear():
    msgs = [
        {"id": 1, "role": "user", "content": "a"},
        {"id": 2, "role": "assistant", "content": "b"},
        {"id": 3, "kind": "fold", "role": "user", "content": "f1"},
        {"id": 4, "role": "user", "content": "c"},
        {"id": 5, "kind": "fold", "role": "user", "content": "f2"},
    ]
    out = caching.mark(msgs, 2)
    assert [m["id"] for m in out if m.get("cache_primer")] == [2]
    assert [m["id"] for m in out if m.get("cache_fold")] == [5]


def test_mark_with_no_primer_or_no_fold_flags_nothing_extra():
    msgs = [{"id": 1, "role": "user", "content": "a"}]
    out = caching.mark(msgs, None)
    assert not any("cache_primer" in m or "cache_fold" in m for m in out)


def test_a_clear_row_counts_as_a_fold_for_the_marker():
    msgs = [{"id": 1, "kind": "fold"}, {"id": 2, "kind": "clear"}]
    out = caching.mark(msgs, None)
    assert [m["id"] for m in out if m.get("cache_fold")] == [2]


# ── Python-only unit tests ──────────────────────────────────────────────────


def test_mark_returns_new_messages_and_never_mutates_its_input():
    src = [
        {"id": 1, "role": "user", "content": "a"},
        {"id": 2, "kind": "fold", "role": "user", "content": "f"},
    ]
    out = caching.mark(src, 1)
    assert not any("cache_primer" in m or "cache_fold" in m for m in src)
    flagged = [m for m in out if m.get("cache_primer") or m.get("cache_fold")]
    assert flagged and all(m is not src[0] and m is not src[1] for m in flagged)


def test_settings_reads_both_and_raises_on_either():
    assert caching.settings({}) == ("cleanup", None)
    with pytest.raises(ValueError):
        caching.settings({"caching": "always"})
    with pytest.raises(ValueError):
        caching.settings({"cache_primer_until": 0})


def test_unsupported_names_the_backend():
    assert str(caching.unsupported("x_backend")) == (
        "x_backend: caching=provider_cache is not supported by this backend "
        "— use none or cleanup"
    )


def test_the_hints_are_the_rust_text():
    assert caching.CACHING_HINT == (
        "how the conversation is kept in the model's view: none = every stored row, "
        "in order, never trimmed (append-only); cleanup = trim superseded reads, "
        "repeated reply bodies and used docs from the view; provider_cache = none's "
        "history plus the provider's cache markers (the end of the primer row named "
        "by cache_primer_until, and the end of the last fold); provider_cache is "
        "refused by a backend that cannot cache. A switch keeps the view already "
        "written: a row a cleanup left out of it stays out until a context_clear "
        "starts a new thread"
    )
    assert caching.CACHE_PRIMER_HINT == (
        "a row id: the row that ends the stable prefix. The primer and every row "
        "before it are pinned — context_fold will not fold a range that starts at or "
        "before it, in any mode. The provider is told where the prefix ends (the end "
        "of that row's text) only under caching=provider_cache; absent, nothing is "
        "pinned or marked"
    )


def test_render_for_model_carries_the_cache_flags_and_nothing_else():
    flagged = {
        "role": "user",
        "content": "a",
        "id": 3,
        "kind": "fold",
        "cache_fold": True,
    }
    assert core._render_for_model([flagged]) == [
        {"role": "user", "content": "a", "cache_fold": True}
    ]
    plain = {"role": "user", "content": "b", "id": 4}
    assert core._render_for_model([plain]) == [{"role": "user", "content": "b"}]


def test_a_backend_declares_no_provider_cache_by_default():
    assert _cfg().provider_cache is False


# ── ported from tests_caching.rs ────────────────────────────────────────────


def test_caching_is_an_enum_that_defaults_to_cleanup_and_refuses_anything_else():
    sch = schema([])
    f = _field(sch, "caching")
    assert f["type"] == "enum"
    assert f["enum"] == ["none", "cleanup", "provider_cache"]
    assert f["default"] == "cleanup"
    for m in ("none", "cleanup", "provider_cache"):
        assert validate(sch, "x", {"caching": m}, {"memory_store_id": "m"}) is None
    err = validate(sch, "x", {"caching": "always"}, {"memory_store_id": "m"})
    assert err is not None
    assert "caching='always' is not one of the legal values" in err
    assert _field(sch, "cache_primer_until")["type"] == "int"
    names = [f["name"] for f in AI_FIELDS]
    assert names.index("caching") == names.index("recent_n") + 1


def test_cleanup_trims_the_view_and_none_and_provider_cache_keep_every_row():
    k = _k()
    _put(k, "user", "read the state")
    _put(k, "assistant", "ok")
    _put(k, "user", _reply(1, "reflect", "old state"))
    _put(k, "assistant", "ok")
    _put(k, "user", _reply(2, "reflect", "new state"))
    _put(k, "assistant", "ok")
    _put(k, "user", _reply(3, "read", _big()))
    _put(k, "assistant", "ok")
    _put(k, "user", _reply(4, "read", _big()))
    _put(k, "assistant", "ok")
    _put(k, "user", _docs(30))
    _put(k, "assistant", "ok")

    stored = _stored(k)
    cleaned = _history(
        core._run and None or []  # placeholder replaced below
    )
    cleaned = _history(__import__("asyncio").run(_view(k, "cleanup")))
    assert not any("old state" in c for _, c in cleaned)
    assert any("[identical to the reply in row" in c for _, c in cleaned)
    assert not any("<start>args" in c for _, c in cleaned)
    assert len(cleaned) == len(stored) - 1
    import asyncio

    assert _history(asyncio.run(_view(k, "none"))) == stored
    assert _history(asyncio.run(_view(k, "provider_cache"))) == stored


def test_none_and_provider_cache_are_append_only_each_turn_a_prefix_of_the_next():
    import asyncio

    k = _k()
    for turn in range(3):
        before = _history(asyncio.run(_view(k, "none")))
        _put(k, "user", f"question {turn}")
        _put(k, "assistant", f"answer {turn}")
        _put(k, "user", _reply(turn, "read", _big()))
        _put(k, "assistant", "ok")
        now = _history(asyncio.run(_view(k, "none")))
        assert now[: len(before)] == before
    tail = _history(asyncio.run(_view(k, "none")))[-12:]
    assert sum(1 for _, c in tail if c.startswith("[reply to ")) == 3


def test_provider_cache_flags_the_primer_end_and_the_last_fold_and_the_fold_moves():
    import asyncio

    k = _k()
    _put(k, "user", "u1")
    a1 = _put(k, "assistant", "a1")
    _put(k, "user", "u2")
    a2 = _put(k, "assistant", "a2")
    _put(k, "user", "u3")
    a3 = _put(k, "assistant", "a3")
    f1 = _fold(k, [2, 4], "fold one")
    f2 = _fold(k, [5, 6], "fold two")

    msgs = caching.mark(asyncio.run(_view(k, "provider_cache")), a1)
    assert [m["id"] for m in msgs if m.get("cache_primer")] == [a1]
    assert [m["id"] for m in msgs if m.get("cache_fold")] == [f2]
    assert any(m.get("id") == f1 for m in msgs)

    _put(k, "user", "u4")
    _put(k, "assistant", "a4")
    f3 = _fold(k, [8, 9], "fold three")
    msgs = caching.mark(asyncio.run(_view(k, "provider_cache")), a1)
    assert [m["id"] for m in msgs if m.get("cache_primer")] == [a1]
    assert [m["id"] for m in msgs if m.get("cache_fold")] == [f3]
    assert a2 and a3


def test_no_primer_field_flags_no_primer_and_no_fold_flags_no_fold():
    import asyncio

    k = _k()
    _put(k, "user", "u1")
    _put(k, "assistant", "a1")
    msgs = caching.mark(asyncio.run(_view(k, "provider_cache")), None)
    assert not any(m.get("cache_primer") or m.get("cache_fold") for m in msgs)


def test_a_primer_outside_the_view_flags_nothing():
    import asyncio

    k = _k()
    _put(k, "user", "u1")
    a1 = _put(k, "assistant", "a1")
    _put(k, "user", "[cleared · 2 rows]", kind="clear")
    _put(k, "user", "u2")
    _put(k, "assistant", "a2")
    msgs = caching.mark(asyncio.run(_view(k, "provider_cache")), a1)
    assert not any(
        m.get("cache_primer") and m.get("id") == a1 for m in msgs
    ) or not any(m.get("id") == a1 for m in msgs)


def test_provider_cache_is_refused_by_a_backend_that_cannot_cache(fake_providers):
    k = _k(caching="provider_cache")
    _put(k, "user", "seed")
    core._recall_due.add("ai")
    before = len(k.rows_for("ai"))
    with pytest.raises(ValueError) as e:
        import asyncio

        asyncio.run(_turn(k, _cfg(), fake_providers, "hi"))
    assert (
        "mock_backend: caching=provider_cache is not supported by this backend"
        in str(e.value)
    )
    assert len(k.rows_for("ai")) == before
    assert "ai" in core._recall_due
    core._recall_due.discard("ai")


def test_a_provider_cache_turn_hands_the_provider_the_primer_and_the_last_fold_flagged(
    fake_providers,
):
    import asyncio

    cfg = _cfg("mock_caching", True)
    k = _k()
    _put(k, "user", "u1")
    a1 = _put(k, "assistant", "a1")
    _put(k, "user", "u2")
    _put(k, "assistant", "a2")
    _put(k, "user", "u3")
    _put(k, "assistant", "a3")
    _put(k, "user", "task")
    k.record["cache_primer_until"] = a1
    k.record["caching"] = "provider_cache"
    _fold(k, [2, 4], "fold one")
    _fold(k, [5, 6], "fold two")

    msgs = asyncio.run(_turn(k, cfg, fake_providers))
    primed = [m["content"] for m in msgs if m.get("cache_primer")]
    assert primed == ["a1"]
    folded = [m for m in msgs if m.get("cache_fold")]
    assert len(folded) == 1 and '"fold two"' in folded[0]["content"]
    assert any('"fold one"' in m["content"] for m in msgs)


def test_a_cleanup_or_none_turn_hands_the_provider_no_flag_even_with_a_primer_and_folds(
    fake_providers,
):
    import asyncio

    for mode in ("cleanup", "none"):
        cfg = _cfg("mock_caching", True)
        k = _k()
        _put(k, "user", "u1")
        a1 = _put(k, "assistant", "a1")
        _put(k, "user", "u2")
        _put(k, "assistant", "a2")
        _put(k, "user", "task")
        k.record["cache_primer_until"] = a1
        k.record["caching"] = mode
        _fold(k, [2, 4], "fold one")
        msgs = asyncio.run(_turn(k, cfg, fake_providers))
        assert not any(m.get("cache_primer") or m.get("cache_fold") for m in msgs)
        assert len(msgs) > 3
        _reset()


def test_a_mode_switch_keeps_the_rows_a_cleanup_left_out_of_the_view_array():
    import asyncio

    k = _k()
    r0 = _put(k, "user", _reply(1, "reflect", "old state"))
    a1 = _put(k, "assistant", "ok")
    r2 = _put(k, "user", _reply(2, "reflect", "new state"))
    a2 = _put(k, "assistant", "ok")
    u5 = _put(k, "user", "u5")
    a5 = _put(k, "assistant", "a5")
    assert r0
    asyncio.run(
        core._save_view(
            "ai",
            k,
            [{"id": i, "res": "full"} for i in (a1, r2, a2, u5, a5)],
            budget_=0,
            why="test",
        )
    )
    hist = _history(asyncio.run(_view(k, "none")))
    assert len(hist) == 5
    assert not any("old state" in c for _, c in hist)
    assert sum(1 for _, c in hist if "new state" in c) == 1


# Not ported (no `context_fold` verb in Python):
#   context_fold_refuses_a_range_that_starts_at_or_before_the_primer
#   context_fold_refuses_a_caching_value_it_cannot_read
