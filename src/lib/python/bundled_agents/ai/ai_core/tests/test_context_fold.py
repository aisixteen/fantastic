"""`context_fold` — the operator verb that folds a chosen stretch of the agent's VIEW
into ONE fold row. Port of Rust `retrieval::context_fold` and tests_context_fold.rs:
the same range rules, the same refusals (each naming its arg), the same view after."""

from __future__ import annotations

import json

import pytest

from ai_core import core, folds
from _fake import FakeCtx


def setup_function(_):
    core._projection.clear()
    core._compaction_mark.clear()
    core._locks.clear()


def _cfg():
    return core.Backend(sentence="s", make_provider=lambda *a: None, name="fake")


def _k(**rec):
    return FakeCtx({"context_window": 32000, "output_reserve": 0, **rec})


def _put(k, role, content, **extra) -> int:
    return k.rows_put("ai", {"role": role, "content": content, **extra})


def _turns(k, texts) -> list[int]:
    """A user row and an assistant answer per text, in order; returns the row ids."""
    ids: list[int] = []
    for t in texts:
        ids.append(_put(k, "user", t))
        ids.append(_put(k, "assistant", f"ok {t}"))
    return ids


def _fold_row(k, fid) -> dict:
    return next(r for r in k.rows_for("ai") if r.get("id") == fid)


async def _view_ids(k) -> list[int]:
    view = await core._load_view("ai", k)
    return [e["id"] for e in view["ids"]]


async def _messages(k):
    return (await core._assemble("ai", "next", k, "c", system_prompt="sys"))[0]


class _Summ:
    """A provider whose every call is counted; a summary request gets `reply`."""

    def __init__(self, reply="TITLE of the span\n\nbody of the span"):
        self.reply = reply
        self.calls = 0

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        yield self.reply


async def _fold(k, payload, cfg=None):
    return await core._make_context_fold(cfg or _cfg())("ai", payload, k)


# ── the default range and the verb's shape ──────────────────────────


async def test_default_range_is_the_rows_after_the_last_clear(fake_providers):
    k = _k()
    _turns(k, ["old one", "old two"])
    clear = await core._context_clear("ai", {"why": "next task"}, k)
    ids = [_put(k, "user", t) for t in ["task one", "task two", "task three"]]

    out = await _fold(k, {"summary": "the first task\n\nDONE: all three"})
    assert out["rows"] == 3, out
    assert out["summarized"] is True, out
    fid = out["folded"]
    fold = _fold_row(k, fid)
    assert fold["kind"] == "fold"
    assert fold["refs"] == ids, "the members are the rows after the clear"
    assert folds.level(k.rows_for("ai"), fold) == 1
    assert fold["content"] == (
        f'[fold {fid} · 3 refs · L1] "the first task"\nthe first task\n\nDONE: all three'
    )
    # the view: the clear, then the fold — nothing after the fold
    assert await _view_ids(k) == [clear["cleared"], fid]
    msgs = await _messages(k)
    assert msgs[2]["content"].startswith("[fold ")
    assert len(msgs) == 4, f"system, clear, fold, the next turn: {msgs}"


async def test_explicit_middle_range_leaves_the_rows_around_it_in_place(fake_providers):
    k = _k()
    ids = _turns(k, ["a", "b", "c"])
    before = await _messages(k)

    out = await _fold(
        k,
        {
            "from": ids[2],
            "to": ids[3],
            "summary": "the lexer work\n\nthe lexer splits lines",
        },
    )
    assert out["rows"] == 2, out
    fid = out["folded"]

    after = await _messages(k)
    assert after[:3] == before[:3], "the prefix before the fold is unchanged"
    assert after[3]["content"].startswith(f"[fold {fid} ·")
    assert after[4:] == before[5:], "the rows after the range stay in place"
    assert await _view_ids(k) == [ids[0], ids[1], fid, ids[4], ids[5]]
    fold = _fold_row(k, fid)
    assert fold["refs"] == [ids[2], ids[3]]
    assert fold["anchor"] == ids[2], "anchored at the smallest member id"
    stored = {r["id"] for r in k.rows_for("ai")}
    assert set(ids) <= stored, "nothing deleted"


# ── summaries ───────────────────────────────────────────────────────


async def test_given_summary_is_verbatim_with_no_provider_call(fake_providers):
    k = _k()
    ids = _turns(k, ["x", "y"])
    prov = fake_providers["ai"] = _Summ()
    summary = "  Verbatim title \n\n  exact body, kept as given  "

    out = await _fold(k, {"summary": summary})
    assert out["summarized"] is True, out
    assert prov.calls == 0, "the summarizer was not asked"
    fid = out["folded"]
    fold = _fold_row(k, fid)
    assert fold["body"] == summary, "the body is the summary, verbatim"
    assert fold["title"] == "Verbatim title"
    assert fold["refs"] == ids
    assert fold["content"] == (
        f'[fold {fid} · 4 refs · L1] "Verbatim title"\n{summary}'
    )


async def test_absent_summary_calls_summarizer_once_and_empty_summary_degrades(
    fake_providers,
):
    k = _k()
    ids = _turns(k, ["p", "q"])
    prov = fake_providers["ai"] = _Summ(
        "Summarised title\n\nASKED: the parser\nDONE: read it"
    )

    out = await _fold(k, {})
    assert prov.calls == 1, f"the summarizer ran once: {out}"
    assert out["summarized"] is True, out
    assert out["rows"] == 4
    fid = out["folded"]
    fold = _fold_row(k, fid)
    assert fold["refs"] == ids
    assert fold["content"].startswith(
        f'[fold {fid} · 4 refs · L1] "Summarised title"\nASKED: the parser'
    ), fold
    assert "INDEX (mechanical):" in fold["body"], "the index rides beside the summary"

    # a summarizer that gives nothing folds the titles and says so
    k2 = _k()
    _turns(k2, ["r", "s"])
    fake_providers["ai"] = _Summ("   ")
    out2 = await _fold(k2, {})
    assert out2["summarized"] is False, out2
    fold2 = _fold_row(k2, out2["folded"])
    assert "(summary unavailable" in fold2["content"], fold2


# ── refusals: each names the arg ────────────────────────────────────


async def test_range_refusals_name_the_arg(fake_providers):
    # an empty view: no rows at all
    k0 = _k()
    empty = await _fold(k0, {"summary": "s"})
    assert empty["reason"] == "bad_arg" and empty["arg"] == "from", empty
    assert "the range is empty" in empty["error"], empty

    k = _k()
    ids = _turns(k, ["a", "b", "c"])
    await core._context_clear("ai", {"why": "x"}, k)
    live = _turns(k, ["d", "e"])
    # an id before the clear is in the store, not in the view
    outside = await _fold(k, {"from": ids[0], "summary": "s"})
    assert outside["reason"] == "bad_arg" and outside["arg"] == "from", outside
    assert "a row id in this agent's view" in outside["error"], outside
    unknown_to = await _fold(k, {"to": 999999, "summary": "s"})
    assert unknown_to["arg"] == "to", unknown_to

    # to before from
    back = await _fold(k, {"from": live[2], "to": live[0], "summary": "s"})
    assert back["reason"] == "bad_arg" and back["arg"] == "to", back
    assert "to comes before from" in back["error"], back

    # a range that crosses a fold row
    k2 = _k()
    ids2 = _turns(k2, ["u1", "u2", "u3"])
    mid = await _fold(k2, {"from": ids2[2], "to": ids2[3], "summary": "middle"})
    crosses = await _fold(k2, {"from": ids2[0], "to": ids2[4], "summary": "s"})
    assert crosses["reason"] == "bad_arg" and crosses["arg"] == "from", crosses
    assert "crosses a fold or clear row" in crosses["error"], crosses
    assert mid["rows"] == 2

    # a range that crosses a clear row is refused the same way
    k3 = _k()
    _turns(k3, ["c1", "c2"])
    clear = await core._context_clear("ai", {"why": "x"}, k3)
    after = _turns(k3, ["c3"])
    crossing_clear = await _fold(
        k3, {"from": clear["cleared"], "to": after[-1], "summary": "s"}
    )
    assert crossing_clear["reason"] == "bad_arg", crossing_clear
    assert crossing_clear["arg"] == "from", crossing_clear

    # the default range after a fold that ends the view is empty
    k4 = _k()
    _turns(k4, ["d1", "d2"])
    whole = await _fold(k4, {"summary": "all of it"})
    assert "folded" in whole, whole
    again = await _fold(k4, {"summary": "s"})
    assert again["reason"] == "bad_arg" and again["arg"] == "to", again

    # bad argument types, each refused by name
    k5 = _k()
    _turns(k5, ["e1"])
    for payload, arg in [
        ({"from": "abc", "summary": "s"}, "from"),
        ({"to": True, "summary": "s"}, "to"),
        ({"summary": 5}, "summary"),
        ({"summary": "   "}, "summary"),
    ]:
        out = await _fold(k5, payload)
        assert out["reason"] == "bad_arg", (payload, out)
        assert out["arg"] == arg, (payload, out)


# ── a turn running for the agent ────────────────────────────────────


async def test_running_turn_refuses_with_locked(fake_providers):
    k = _k()
    _turns(k, ["x"])
    lock = core._lock_for("ai")
    await lock.acquire()
    try:
        out = await _fold(k, {"summary": "s"})
    finally:
        lock.release()
    assert out["reason"] == "locked", out
    assert not any(r.get("kind") == "fold" for r in k.rows_for("ai")), "no fold row"


# ── history and grep reach the members ──────────────────────────────


async def test_history_fold_and_grep_reach_members(fake_providers):
    k = _k()
    ids = [
        _put(k, "user", "first"),
        _put(k, "assistant", "second"),
        _put(k, "user", "third"),
        _put(k, "assistant", "the secret is 9f3a"),
        _put(k, "user", "last"),
    ]
    out = await _fold(k, {"from": ids[1], "to": ids[3], "summary": "middle"})
    fid = out["folded"]

    h = await core._make_history(None)("ai", {"fold": fid}, k)
    assert h["fold"]["id"] == fid, h
    assert h["fold"]["summary"] == "middle", h
    assert [d["id"] for d in h["rows"]] == [ids[1], ids[2], ids[3]], h

    g = await core._grep("ai", {"query": "secret"}, k)
    assert g["total"] == 1, g
    assert g["matches"][0]["id"] == ids[3], g


async def test_a_view_array_naming_a_missing_row_is_no_view(fake_providers):
    # A stale array (a row the store no longer has) is not the view: the head is.
    k = _k()
    ids = _turns(k, ["one", "two", "three"])
    await core._save_view(
        "ai", k, [{"id": 999999, "res": "full"}], budget_=1000, why="test"
    )

    out = await _fold(k, {"summary": "all three"})
    assert out["rows"] == len(ids), out
    assert _fold_row(k, out["folded"])["refs"] == ids


@pytest.mark.xfail(
    strict=True,
    reason="Python re-renders the prefix untrimmed after a fold; keeping the trimmed "
    "bytes needs a view-entry field the issue does not name. Rust is the spec.",
)
async def test_the_prefix_keeps_the_trimmed_bytes_the_model_saw_before_the_fold(
    fake_providers,
):
    # The model's view trims the store: a whole-state read a later one supersedes is
    # dropped, and a body repeated from an earlier reply is stubbed. A fold must not
    # bring either back: the messages before the range stay byte-identical.
    k = _k()
    long = "the same long reply body, repeated word for word. " * 10
    old = "[reply to 1 · notes reflect]\nthe old state"
    superseded = _put(k, "user", old)
    _put(k, "assistant", "noted")
    _put(k, "user", "[reply to 2 · notes reflect]\nthe new state")
    _put(k, "assistant", "noted again")
    _put(k, "user", f"[reply to 3 · notes read]\n{long}")
    _put(k, "assistant", "read it")
    _put(k, "user", f"[reply to 4 · notes read]\n{long}")
    _put(k, "assistant", "and again")
    ids = []
    for t in (
        "question about the range",
        "answer about the range",
        "question after the range",
        "answer after the range",
    ):
        ids.append(_put(k, "user" if t.startswith("question") else "assistant", t))
    before = await _messages(k)
    # the trimmers did their work: the superseded read is gone, the repeat is a stub
    assert not any(m["content"] == old for m in before)
    assert any("[identical to the reply in row" in m["content"] for m in before)
    kpos = next(
        i for i, m in enumerate(before) if m["content"] == "question about the range"
    )

    out = await _fold(k, {"from": ids[0], "to": ids[1], "summary": "the range, folded"})
    assert out["rows"] == 2, out
    fid = out["folded"]

    after = await _messages(k)
    f = next(
        i for i, m in enumerate(after) if m["content"].startswith(f"[fold {fid} ·")
    )
    assert f == kpos, "the fold sits where the range began"
    assert json.dumps(after[:f]) == json.dumps(before[:kpos]), "prefix byte-identical"
    assert json.dumps(after[f + 1 :]) == json.dumps(before[kpos + 2 :]), (
        "the rows after the range are byte-identical"
    )
    assert superseded not in await _view_ids(k), "a dropped row stays out of the view"
