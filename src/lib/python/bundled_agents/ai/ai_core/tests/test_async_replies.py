"""FULL ASYNC: a reply is a row, not a turn.

A settled call's body is inlined only if it is small; otherwise it is stored under its
pid and the model is handed the digest. Then a 240KB ledger dump is a row to page, never
a turn that has to fit — which is the difference between "the model can look" and "the
chain is dead". These tests are the store, the cap, and the paging.
"""

from __future__ import annotations

import json

from ai_core.context import CHARS_PER_TOKEN, budget, estimate_one
from _fake import FakeCtx as _Leg  # noqa: E402
from ai_core.core import (
    _inline_cap,
    _load_reply,
    _pull,
    _pull_cap_chars,
    _store_reply,
)


def test_a_pull_page_can_never_exceed_the_inline_cap():
    """THE INVARIANT that closes the recursion. If a page could be larger than what
    inlines, the page itself would be stored and digested, and the model would pull
    the pull forever (measured in review before it reached a live run)."""
    for window in (8000, 12000, 32000, 131072):
        rec = {"context_window": window, "output_reserve": 0}
        assert _inline_cap(rec) == max(256, budget(rec) // 8)
        assert _pull_cap_chars(rec) // CHARS_PER_TOKEN <= _inline_cap(rec)


async def test_store_then_pull_pages_through_the_body():
    k = _Leg()
    body = "".join(f"line {i}\n" for i in range(8000))  # ~70KB, cap is ~24KB chars
    assert await _store_reply("ai", k, 1001, body)
    cap = _pull_cap_chars(k.get("ai"))
    got, off, pages = "", 0, 0
    while True:
        r = await _pull("ai", {"pid": 1001, "offset": off}, k)
        assert "error" not in r, r
        assert len(r["text"]) <= cap
        got += r["text"]
        pages += 1
        if r["eof"]:
            break
        off = r["next_offset"]
    assert got == body
    assert pages > 1, "the body should have needed more than one page"


async def test_a_pull_can_never_return_more_than_the_cap():
    k = _Leg()
    await _store_reply("ai", k, 1002, "z" * 100_000)
    cap = _pull_cap_chars(k.get("ai"))
    r = await _pull("ai", {"pid": 1002, "length": cap + 1}, k)
    assert r["reason"] == "bad_arg" and r["arg"] == "length"
    r = await _pull("ai", {"pid": 1002}, k)
    # a page is sized so the SERIALIZED reply fits inline — never more than the cap in
    # chars, and shorter when escaping would push the reply over
    assert 64 < len(r["text"]) <= cap and not r["eof"]
    assert estimate_one({"role": "user", "content": json.dumps(r)}) <= _inline_cap(
        k.get("ai")
    )


async def test_pulling_an_unknown_pid_is_a_readable_refusal():
    r = await _pull("ai", {"pid": 999999}, _Leg())
    assert r["reason"] == "not_found"
    r = await _pull("ai", {}, _Leg())
    assert r["reason"] == "bad_arg" and r["arg"] == "pid"


async def test_no_file_leg_means_the_body_cannot_be_stored():
    k = _Leg()
    assert await _load_reply("ai", k, "p:1", 0, 10) is None


# ── grep: find the line, don't page for it ───────────────────────────


async def test_grep_finds_a_line_inside_a_stored_body_without_pulling_it(monkeypatch):
    from ai_core import core

    k = _Leg()
    body = "\n".join(
        ["chunk 5: " + "the quick brown fox " * 40] * 3
        + [">> audit marker: a1b2c3d4"]
        + ["the quick brown fox " * 40] * 3
    )
    await _store_reply("ai", k, 55, body)
    rows = [
        {"role": "user", "content": "survey the corpus", "id": "user:0001"},
        {
            "role": "assistant",
            "content": "Reading filler_5.",
            "id": "ai:0002",
            "parent": "user:0001",
        },
        {
            "role": "user",
            "content": '[reply to 55 · notes read · "survey filler_5"]\n(settled, 9000 bytes — NOT inlined. Pull a page: …)',
            "id": "ai:0003",
        },
    ]

    async def fake_load(self_id, kernel, behind=False):
        return rows

    monkeypatch.setattr(core, "_load_history", fake_load)
    r = await core._grep("ai", {"query": "AUDIT MARKER"}, k)
    assert r["total"] == 1 and not r["truncated"]
    m = r["matches"][0]
    assert m["id"] == 55 and m["line"] == 4 and "a1b2c3d4" in m["text"]


async def test_grep_scopes_to_ids_and_caps_matches(monkeypatch):
    from ai_core import core

    k = _Leg()
    await _store_reply("ai", k, 1, "\n".join(f"hit {i}" for i in range(50)))
    await _store_reply("ai", k, 2, "hit elsewhere")

    async def fake_load(self_id, kernel, behind=False):
        return [
            {
                "role": "user",
                "content": "[reply to 1 · n r]\nx\n\n[reply to 2 · n r]\ny",
                "id": "u:0",
            }
        ]

    monkeypatch.setattr(core, "_load_history", fake_load)
    r = await core._grep("ai", {"query": "hit", "ids": [1], "limit": 5}, k)
    assert r["total"] == 50 and len(r["matches"]) == 5 and r["truncated"]
    assert all(m["id"] == 1 for m in r["matches"])


async def test_grep_refuses_without_a_query():
    from ai_core.core import _grep

    r = await _grep("ai", {}, _Leg())
    assert r["reason"] == "bad_arg" and r["arg"] == "query"


async def test_grep_puts_the_stored_body_hit_first_and_collapses_repeated_lines():
    """Measured 2026-09-06: the task line repeated per delivery matched 28 times and
    pushed the one body hit past the cap. Bodies come first; identical lines once."""
    from ai_core.core import _grep, _store_reply

    k = _Leg()
    for i in range(6):
        k.rows_put(
            "ai", {"role": "user", "content": "find the audit marker in the files"}
        )
    k.rows_put(
        "ai",
        {
            "role": "user",
            "content": "[reply to 7 · notes read]\n(settled, 9 bytes — NOT inlined)",
        },
    )
    await _store_reply("ai", k, 7, "chunk 1\n>> audit marker: deadbeef\nchunk 1")
    r = await _grep("ai", {"query": "audit marker"}, k)
    assert r["matches"][0]["id"] == 7 and "deadbeef" in r["matches"][0]["text"]
    assert r["total"] == 2, r


async def test_grep_scoped_to_a_fold_searches_its_members_bodies():
    """A descent: `ids:[fold]` must reach the bodies behind the member replies."""
    from ai_core.core import _grep, _store_reply
    from ai_core.folds import make_fold

    k = _Leg()
    call = k.mint_id()
    pid = k.mint_id()
    reply = k.rows_put(
        "ai",
        {
            "role": "user",
            "parent": call,
            "content": f'[reply to {pid} · notes read · "read filler_1" · ↑{call}]\n(settled, 9 bytes — NOT inlined)',
        },
    )
    await _store_reply("ai", k, pid, "chunk 1\n>> audit marker: cafebabe\nchunk 1")
    fold = make_fold(
        "ai", [call, reply], "read filler_1", "plain filler", False, id=k.mint_id()
    )
    k.rows_put("ai", fold)
    r = await _grep("ai", {"query": "audit marker", "ids": [fold["id"]]}, k)
    assert (
        r["total"] == 1
        and r["matches"][0]["id"] == pid
        and "cafebabe" in r["matches"][0]["text"]
    )
    r2 = await _grep("ai", {"query": "audit marker", "ids": [pid]}, k)
    assert r2["total"] == 1, "an id that is itself a pid still reaches its body"


async def test_grep_width_is_the_models_to_raise():
    from ai_core.core import _grep, _store_reply

    k = _Leg()
    pid = k.mint_id()
    k.rows_put(
        "ai",
        {
            "role": "user",
            "content": f"[reply to {pid} · notes read]\n(settled, 9 bytes — NOT inlined)",
        },
    )
    await _store_reply("ai", k, pid, "rule: " + "x" * 300 + " VALUE=cafebabe")
    r = await _grep("ai", {"query": "rule:"}, k)
    assert (
        len(r["matches"][0]["text"]) == 200
        and "cafebabe" not in r["matches"][0]["text"]
    )
    r = await _grep("ai", {"query": "rule:", "width": 400}, k)
    assert "cafebabe" in r["matches"][0]["text"]


async def test_grep_falls_back_per_knob_and_pull_refuses_a_negative_length():
    """2026-09-16: a bad `limit` left `width` unbound (NameError on the first
    match); a negative `length` sliced from the end with a negative next_offset."""
    from ai_core.core import _grep, _pull, _store_reply

    k = _Leg()
    pid = k.mint_id()
    k.rows_put(
        "ai",
        {
            "role": "user",
            "content": f"[reply to {pid} · notes read]\n(settled — NOT inlined)",
        },
    )
    await _store_reply("ai", k, pid, "rule: " + "x" * 300 + " VALUE=cafebabe")
    r = await _grep("ai", {"query": "rule:", "limit": "nope"}, k)
    assert r["matches"] and len(r["matches"][0]["text"]) == 200
    r = await _pull("ai", {"pid": pid, "length": -5}, k)
    assert r.get("reason") == "bad_arg" and "negative" in r["error"]
