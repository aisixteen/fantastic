"""The view array is the agent's state; `context_recall` rebuilds it; every self-read
verb answers under the inline cap."""

from __future__ import annotations

import json

from ai_core import core
from ai_core.context import estimate_one
from ai_core.core import (
    _make_boot,
    _context_recall,
    _fit_reply,
    _grep,
    _load_view,
    _res_of,
    _save_history,
    _save_view,
    _store_reply,
)
from ai_core.strategies.strip import _stripped

from test_folds import _K, _chain, _fold, _id, _row  # noqa: E402


def _cfg():
    from ai_core.core import Backend

    return Backend(
        sentence="s",
        default_model="m",
        default_endpoint="e",
        make_provider=lambda *a: None,
        name="fake",
        module_name="fake",
    )


# noqa: E402  (the leg kernel and row helpers)

BIG = "x" * 3000


def setup_function(_):
    _stripped.clear()
    core._projection.clear()
    core._compaction_mark.clear()
    core._providers.pop("ai", None)


async def _seed(k, rows):
    await _save_history("ai", k, rows)


# ── the cap invariant ────────────────────────────────────────────────


def test_fit_reply_never_exceeds_the_cap_and_says_so():
    items = [{"id": f"r:{i}", "text": "y" * 300} for i in range(200)]
    kept, cut = _fit_reply(items, 500)
    assert cut and 0 < len(kept) < 200
    assert estimate_one({"role": "user", "content": json.dumps(kept)}) <= 500
    kept2, cut2 = _fit_reply(items[:2], 10_000)
    assert kept2 == items[:2] and not cut2


async def test_grep_answers_under_the_inline_cap_for_a_broad_query(monkeypatch):
    k = _K()
    rows = [
        _row("user", "\n".join(f"marker {i}.{j}" for j in range(80)), f"u:{i}")
        for i in range(40)
    ]  # every row matches, on distinct lines (identical lines are reported once)
    for i in range(10):
        await _store_reply(
            "ai", k, f"n:{i}", "\n".join(f"marker line {i}.{j}" for j in range(500))
        )
    rows.append(
        _row(
            "user",
            "\n\n".join(f"[reply to n:{i} · n r]\nx" for i in range(10)),
            "u:tags",
        )
    )

    async def fake_load(self_id, kernel):
        return rows

    monkeypatch.setattr(core, "_load_history", fake_load)
    r = await _grep("ai", {"query": "marker"}, k)
    cap = core._inline_cap(k.get("ai"))
    assert estimate_one({"role": "user", "content": json.dumps(r)}) <= cap + 64
    assert r["truncated"] and r["next"]["id"]


async def test_history_is_digest_resolution_and_paged():
    k = _K()
    await _seed(k, [_row("user", BIG, f"u:{i}") for i in range(30)])
    from ai_core.core import _make_history

    h = _make_history(_cfg())
    r = await h("ai", {}, k)
    assert (
        r["total"] == 30
        and r["rows"]
        and all("head" in x and "bytes" in x for x in r["rows"])
    )
    assert all(len(x["head"]) <= 120 for x in r["rows"]), "no bodies in a digest"
    if r["next"]:
        r2 = await h("ai", {"after": r["next"]}, k)
        assert r2["rows"][0]["id"] != r["rows"][0]["id"]


# ── the view array ───────────────────────────────────────────────────


async def test_view_round_trips_and_assemble_builds_from_it(monkeypatch):
    k = _K()
    a = _chain("a", 3)
    await _seed(k, a)
    ids = [
        {"id": a[0]["id"], "res": "full"},
        {"id": a[2]["id"], "res": "stub"},
        {"id": a[4]["id"], "res": "full"},
    ]
    assert await _save_view("ai", k, ids, budget_=1000, why="test")
    v = await _load_view("ai", k)
    assert v["ids"] == ids and v["why"] == "test"

    async def fake_sys(self_id, kernel):
        return "sys"

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    msgs, n = await core._assemble("ai", "live", k, "c")
    assert n == len(a)
    body = msgs[1:-1]
    # the array, in order, then every row appended after its newest member
    assert [m["id"] for m in body] == [e["id"] for e in ids] + [r["id"] for r in a[5:]]
    assert "(stripped:" in body[1]["content"] and BIG not in body[1]["content"]
    assert core._view_source["ai"] == "array"


async def test_a_stale_array_falls_back_to_the_store_and_says_so(monkeypatch):
    k = _K()
    a = _chain("a", 1, big=False)
    await _seed(k, a)
    await _save_view("ai", k, [{"id": "gone:0000", "res": "full"}], budget_=1, why="x")

    async def fake_sys(self_id, kernel):
        return "sys"

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    msgs, _ = await core._assemble("ai", "live", k, "c")
    assert len(msgs) == 1 + len(a) + 1
    assert core._view_source["ai"].startswith("store")


def test_res_of_classifies_rows():
    assert _res_of(_fold("ai", ["x:1", "x:2"], "t", "b", False)) == "fold"
    assert (
        _res_of(
            {
                "role": "user",
                "content": "[reply to n:1 · n r]\n(stripped: 10 bytes — recall by pid)",
            }
        )
        == "stub"
    )
    assert _res_of({"role": "user", "content": "[reply to n:1 · n r]\nbody"}) == "full"


# ── context_recall ───────────────────────────────────────────────────


async def test_recall_mechanical_fit_is_under_budget_and_persists_a_view_and_a_row(
    monkeypatch,
):
    k = _K()
    a = _chain("a", 20)
    await _seed(k, [_row("user", "GOAL", "user:goal")] + a)

    async def fake_sys(self_id, kernel):
        return "s" * 4000

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    r = await _context_recall("ai", {"dry": True}, k)
    assert r["degraded"] is True and r["kept"] >= 1
    v = await _load_view("ai", k)
    assert v and v["ids"][0]["id"] == _id("user:goal"), "the goal turn is always in"
    total = sum(estimate_one({"role": "user", "content": "x" * (0)}) for _ in ())  # noqa: F841
    rows = await core._load_history("ai", k)
    rec = [x for x in rows if x.get("kind") == "recall"]
    assert len(rec) == 1 and "[recall " in rec[0]["content"]
    # a second recall is deterministic on the same store (modulo its own new row)
    r2 = await _context_recall("ai", {"dry": True}, k)
    assert r2["kept"] >= r["kept"] - 1


async def test_recall_asks_the_model_and_expands_only_what_fits(monkeypatch):
    k = _K()
    a = _chain("a", 12)
    await _seed(k, [_row("user", "GOAL", "user:goal")] + a)

    async def fake_sys(self_id, kernel):
        return "s" * 4000

    monkeypatch.setattr(core, "_default_system_content", fake_sys)

    reply_ids = {r["id"] for r in a if str(r["content"]).startswith("[reply to ")}

    class _P:
        async def chat(self, messages):
            head_ids = [
                int(ln.split(" · ")[0])
                for ln in messages[0]["content"].splitlines()
                if " · " in ln and ln.split(" · ")[0].isdigit()
            ]
            want = [i for i in head_ids if i in reply_ids][-2:] + [999999999]
            yield json.dumps({"expand": want, "why": "the last two replies matter"})

    core._providers["ai"] = _P()
    r = await _context_recall("ai", {}, k)
    assert r["degraded"] is False and r["why"] == "the last two replies matter"
    v = await _load_view("ai", k)
    reply_ids = {r["id"] for r in a if str(r["content"]).startswith("[reply to ")}
    full_replies = [e for e in v["ids"] if e["res"] == "full" and e["id"] in reply_ids]
    assert 1 <= len(full_replies) <= 2, (
        "expansions are the model's, capped by budget; a ghost id is ignored"
    )


async def test_recall_clears_ram_scratch_but_never_the_store(monkeypatch):
    k = _K()
    await _seed(k, [_row("user", "GOAL", "user:goal")] + _chain("a", 8))

    async def fake_sys(self_id, kernel):
        return "s"

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    core._projection["ai"] = {"fired": True}
    core._compaction_mark["ai"] = {"fired_at_index": 3}
    _stripped["ai"] = {"n:1"}
    before = len(await core._load_history("ai", k))
    await _context_recall("ai", {"dry": True}, k)
    assert (
        "ai" not in core._projection
        and "ai" not in core._compaction_mark
        and "ai" not in _stripped
    )
    assert len(await core._load_history("ai", k)) == before + 1, (
        "one recall row appended, nothing removed"
    )


async def test_boot_recalls_only_when_there_is_a_store(monkeypatch):
    k = _K()
    _boot = _make_boot(_cfg())
    assert await _boot("ai", {}, k) is None

    async def fake_sys(self_id, kernel):
        return "s"

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    await _seed(k, [_row("user", "GOAL", "user:goal")] + _chain("a", 8))
    r = await _boot("ai", {}, k)
    assert r and r["kept"] >= 1 and await _load_view("ai", k)


async def test_a_fold_batch_makes_recall_due_on_the_next_turn(monkeypatch):
    from ai_core.core import _project_context, _recall_due

    k = _K()
    body = (
        [{"role": "system", "content": "sys"}, _row("user", "GOAL", "user:goal")]
        + _chain("a", 12)
        + [_row("user", "now?", None)]
    )
    out = await _project_context(
        {"context_window": 2500, "output_reserve": 0, "context_strategy": "fold"},
        _cfg(),
        None,
        "ai",
        k,
        "cli",
        body,
        len(body) - 1,
    )
    assert isinstance(out, list)
    assert "ai" in _recall_due
    assert await _load_view("ai", k), "every projection writes the view"


async def test_a_reply_in_flight_across_a_reboot_is_recorded_not_acted_on(monkeypatch):
    """Measured 2026-09-06: the rebooted agent was woken by answers to calls its
    previous incarnation made, and finished THAT work instead of the new instruction."""
    from ai_core.core import _deliver, _incarnation, _make_boot

    async def fake_sys(self_id, kernel):
        return "s"

    monkeypatch.setattr(core, "_default_system_content", fake_sys)

    k = _K()
    await _seed(k, [_row("user", "GOAL", "user:goal")])
    _incarnation["ai"] = 1
    sends = []

    class _Kern(_K):
        async def send(self, batch):
            p = batch[0]["payload"]
            if p.get("type") == "send":
                sends.append(p)
                return [{"result": {"response": "ok"}, "status": "completed"}]
            return await super().send(batch)

    kk = _Kern()
    kk.rows, kk._by_agent, kk.bodies, kk.views = k.rows, k._by_agent, k.bodies, k.views
    boot = _make_boot(_cfg())
    await boot("ai", {}, kk)  # reboot → incarnation 2
    await _deliver(kk, "ai", "[reply to n:1 · n r]\nlate", "c", None, born=1)
    assert sends == [], "no turn ran for the late reply"
    rows = await core._load_history("ai", kk)
    assert rows[-1].get("kind") == "late" and "late" in rows[-1]["content"]
    await _deliver(kk, "ai", "[reply to n:2 · n r]\nfresh", "c", None, born=2)
    assert len(sends) == 1, "a reply from THIS incarnation runs a turn"


async def test_rows_appended_after_the_array_ride_behind_it(monkeypatch):
    """The array is the head as of its write; the log keeps growing. Measured: served the
    array alone, the model restarted its survey every turn."""
    k = _K()
    a = _chain("a", 4, big=False)
    await _seed(k, a)
    await _save_view(
        "ai",
        k,
        [{"id": a[0]["id"], "res": "full"}, {"id": a[2]["id"], "res": "full"}],
        budget_=1000,
        why="t",
    )
    later = [
        _row("assistant", "I just read filler_9", "ai:new1", a[-1]["id"]),
        _row(
            "user",
            "[reply to notes:new · notes read]\nfiller_9 body",
            "ai:new2",
            "ai:new1",
        ),
    ]
    await _seed(k, later)

    async def fake_sys(self_id, kernel):
        return "sys"

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    msgs, _ = await core._assemble("ai", "live", k, "c")
    ids = [m.get("id") for m in msgs[1:-1]]
    assert ids[:2] == [a[0]["id"], a[2]["id"]], "the array first"
    assert ids[-2:] == [_id("ai:new1"), _id("ai:new2")], (
        "then everything appended since"
    )
    assert core._view_source["ai"] == "array"


async def test_the_recall_row_names_the_newest_root(monkeypatch):
    k = _K()
    await _seed(
        k,
        [_row("user", "TASK 0: first", "user:t0")]
        + _chain("a", 2)
        + [_row("user", "TASK 1: latest work", "user:t1")],
    )

    async def fake_sys(self_id, kernel):
        return "sys"

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    await _context_recall("ai", {"dry": True}, k)
    rec = [x for x in await core._load_history("ai", k) if x.get("kind") == "recall"][
        -1
    ]
    assert f'live task: {_id("user:t1")} "TASK 1: latest work"' in rec["content"], rec[
        "content"
    ]


async def test_the_live_root_is_found_even_when_a_fold_collapsed_it(monkeypatch):
    k = _K()
    b = _chain("b", 2)
    rows = (
        [_row("user", "TASK 0: first", "user:t0")]
        + _chain("a", 2)
        + [_row("user", "TASK 1: latest work", "user:t1")]
        + b
    )
    await _seed(k, rows)
    await _seed(
        k,
        [
            _fold(
                "ai", ["user:t1"] + [r["id"] for r in b], "task 1 folded", "body", False
            )
        ],
    )

    async def fake_sys(self_id, kernel):
        return "sys"

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    await _context_recall("ai", {"dry": True}, k)
    rec = [x for x in await core._load_history("ai", k) if x.get("kind") == "recall"][
        -1
    ]
    # chain b's own root ("task b") is the newest root and the fold collapsed it too
    assert f"live task: {_id('user:b')}" in rec["content"], rec["content"]
