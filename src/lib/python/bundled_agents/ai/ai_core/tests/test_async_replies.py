"""FULL ASYNC: a reply is a row, not a turn.

A settled call's body is inlined only if it is small; otherwise it is stored under its
pid and the model is handed the digest. Then a 240KB ledger dump is a row to page, never
a turn that has to fit — which is the difference between "the model can look" and "the
chain is dead". These tests are the store, the cap, and the paging.
"""

from __future__ import annotations


from ai_core.context import budget
from _fake import FakeCtx as _Leg  # noqa: E402
from ai_core.core import (
    _inline_cap,
    _load_reply,
    _store_reply,
)


def test_the_inline_cap_is_an_eighth_of_the_budget():
    for window in (8000, 12000, 32000, 131072):
        rec = {"context_window": window, "output_reserve": 0}
        assert _inline_cap(rec) == max(256, budget(rec) // 8)


async def test_a_body_is_held_in_ram_for_this_run_only():
    """BODIES ARE NEVER PERSISTED: a large reply is kept in RAM under its pid for
    this incarnation, and after a restart only its origin line remains."""
    k = _Leg()
    body = "".join(f"line {i}\n" for i in range(800))
    assert await _store_reply("ai", k, 1001, body)
    text, nxt, eof, total = await _load_reply("ai", k, 1001, 0, 10**7)
    assert text == body and eof and total == len(body)
    assert not k.store_calls, "nothing of the body reached the store"
    from ai_core.core import _mem

    _mem(k)["bodies"].pop("ai", None)  # the process restarts
    assert await _load_reply("ai", k, 1001, 0, 10) is None


# ── grep: find the line, don't page for it ───────────────────────────


async def test_grep_finds_the_line_inside_a_body_that_was_never_pulled(monkeypatch):
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
    """A task line repeated per delivery can match many times and push the one body
    hit past the cap. Bodies come first; identical lines once."""
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


async def test_a_fold_id_in_grep_scope_descends_to_its_members():
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
        "ai",
        [call, reply],
        "read filler_1",
        "plain filler",
        False,
        id=k.mint_id(),
        anchor=call,
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


async def test_grep_falls_back_per_knob():
    """A bad `limit` must not leave `width` unbound (NameError on the first match)."""
    from ai_core.core import _grep, _store_reply

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
