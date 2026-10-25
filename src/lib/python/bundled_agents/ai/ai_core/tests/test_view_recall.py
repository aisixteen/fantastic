"""The view array is the agent's state; `context_recall` rebuilds it; every self-read
verb answers under the inline cap."""

from __future__ import annotations

import json

from ai_core import core
from ai_core.context import estimate_one
from ai_core.core import (
    _make_boot,
    _context_recall,
    _is_task_root,
    _standing,
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
        make_provider=lambda *a: None,
        name="fake",
    )


# noqa: E402  (the leg kernel and row helpers)

BIG = "x" * 3000


def setup_function(_):
    _stripped.clear()
    core._projection.clear()
    core._compaction_mark.clear()


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


async def test_grep_answers_under_the_inline_cap_for_a_broad_query():
    k = _K()
    rows = [
        _row("user", "\n".join(f"marker {i}.{j}" for j in range(80)), f"u:{i}")
        for i in range(40)
    ]  # every row matches, on distinct lines (identical lines are reported once)
    pids = [k.mint_id() for _ in range(10)]
    for i, pid in enumerate(pids):
        await _store_reply(
            "ai", k, pid, "\n".join(f"marker line {i}.{j}" for j in range(500))
        )
    rows.append(
        _row(
            "user",
            "\n\n".join(f"[reply to {pid} · n r]\nx" for pid in pids),
            "u:tags",
        )
    )
    await _seed(k, rows)
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


async def test_recall_asks_the_model_and_expands_only_what_fits(
    monkeypatch, fake_providers
):
    k = _K()
    a = _chain("a", 12)
    await _seed(k, [_row("user", "GOAL", "user:goal")] + a)

    async def fake_sys(self_id, kernel):
        return "s" * 4000

    monkeypatch.setattr(core, "_default_system_content", fake_sys)

    reply_ids = {r["id"] for r in a if str(r["content"]).startswith("[reply to ")}

    class _P:
        async def chat(self, messages, max_tokens=None):
            head_ids = [
                int(ln.split(" · ")[0])
                for ln in messages[0]["content"].splitlines()
                if " · " in ln and ln.split(" · ")[0].isdigit()
            ]
            want = [i for i in head_ids if i in reply_ids][-2:] + [999999999]
            yield json.dumps({"expand": want, "why": "the last two replies matter"})

    fake_providers["ai"] = _P()
    r = await _context_recall("ai", {}, k, _cfg())
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


async def test_boot_is_lazy_and_marks_the_view_for_rebuilding():
    """Boot never touches the memory store; it marks the first turn to read the head
    and run `context_recall`."""
    k = _K()
    await _seed(k, [_row("user", "GOAL", "user:goal")] + _chain("a", 8))
    k.store_calls.clear()
    _boot = _make_boot(_cfg())
    assert await _boot("ai", {}, k) is None
    assert k.store_calls == [], "boot read nothing"
    assert "ai" in core._recall_due, "the first turn rebuilds the view"
    core._recall_due.discard("ai")


async def test_boot_without_a_memory_store_fails_loud():
    k = _K({"context_window": 12000, "output_reserve": 0, "memory_store_id": None})
    k.record.pop("memory_store_id")
    out = await _make_boot(_cfg())("ai", {}, k)
    assert out["reason"] == "missing_arg" and out["arg"] == "memory_store_id"


async def test_a_fold_batch_makes_recall_due_on_the_next_turn(monkeypatch):
    from ai_core.core import RECALL_EVERY, _folds_since, _project_context, _recall_due

    k = _K()
    # one overflow is ONE fold now (sized by tokens): the batch that crosses RECALL_EVERY
    _folds_since["ai"] = RECALL_EVERY - 1
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
    """A rebooted agent must not be woken by answers to calls its previous
    incarnation made, finishing THAT work instead of the new instruction."""
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
    kk.rows, kk._by_agent, kk._alias = k.rows, k._by_agent, k._alias
    kk.next_id = k.next_id
    boot = _make_boot(_cfg())
    await boot("ai", {}, kk)  # reboot → incarnation 2
    await _deliver(kk, "ai", "[reply to n:1 · n r]\nlate", "c", None, born=1)
    # the late reply is handed to the TARGET marked `late` — it records the row under
    # its own key (a store keys every row by its writer) and runs no turn
    assert len(sends) == 1 and sends[0].get("late") is True
    out = await core._make_send(_cfg())("ai", sends[0], kk)
    assert out.get("late") is True
    rows = await core._load_history("ai", kk)
    assert rows[-1].get("kind") == "late" and "late" in rows[-1]["content"]
    await _deliver(kk, "ai", "[reply to n:2 · n r]\nfresh", "c", None, born=2)
    assert len(sends) == 2 and not sends[1].get("late"), (
        "a reply from THIS incarnation runs a turn"
    )


async def test_rows_appended_after_the_array_ride_behind_it(monkeypatch):
    """The array is the head as of its write; the log keeps growing. Served the array
    alone, the model would restart its survey every turn."""
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


# ── a head longer than the budget folds at recall ─


class _FoldingModel:
    """Answers a fold summary with a title + body, and the recall's question with
    an empty expansion list."""

    last_stop_reason = None

    def __init__(self):
        self.summaries = 0

    async def chat(self, messages, max_tokens=None):
        if "Summarize the conversation" in str(messages[0].get("content")):
            self.summaries += 1
            yield f"read files {self.summaries}\n\nthe files were read."
        else:
            yield json.dumps({"expand": [], "why": "the folds carry it"})


async def _long_head(k, strategy="fold"):
    """A head written under a window far larger than this agent's: 20 reads of
    3000-char replies (~15k tokens) against a 12k window."""
    k.record["context_strategy"] = strategy
    await _seed(k, [_row("user", "GOAL", "user:goal")] + _chain("a", 20))


async def test_recall_folds_a_head_longer_than_the_budget(monkeypatch, fake_providers):
    k = _K()
    await _long_head(k)

    async def fake_sys(self_id, kernel):
        return "s" * 4000

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    fake_providers["ai"] = model = _FoldingModel()
    r = await _context_recall("ai", {}, k, _cfg())
    folds = k.folds_of()
    # ONE fold for the over-budget head (sized by tokens)
    assert r["folded"] == len(folds) == 1 and model.summaries == 1, r
    assert all(f["refs"] and f["title"] and f["body"] for f in folds)
    # NOTHING FELL OUT UNMARKED: every head row is in the view or under a fold
    v = await _load_view("ai", k)
    in_view = {e["id"] for e in v["ids"]}
    covered = core.expand_fold_ids(
        k.rows_of(), [e["id"] for e in v["ids"] if e["res"] == "fold"]
    )
    turns = [
        x["id"] for x in k.rows_of() if x.get("kind") not in ("fold", "view", "recall")
    ]
    assert all(t in in_view or t in covered for t in turns)
    assert r["dropped"] == 0, r
    assert v["ids"][0]["id"] == _id("user:goal"), "the goal turn is always in"
    assert any(e["res"] == "fold" for e in v["ids"])


async def test_a_recall_fold_tells_the_turns_caller(monkeypatch, fake_providers):
    k = _K()
    await _long_head(k)

    async def fake_sys(self_id, kernel):
        return "s" * 4000

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    fake_providers["ai"] = _FoldingModel()
    r = await _context_recall("ai", {}, k, _cfg(), client_id="c1")
    ev = [p for _, p in k.emitted if p.get("type") == "context"]
    assert len(ev) == 1 and ev[0]["phase"] == "compacted", ev
    assert ev[0]["client_id"] == "c1"
    assert (
        ev[0]["detail"]["folded"] == r["folded"] and ev[0]["detail"]["recall"] is True
    )


async def test_a_dry_recall_or_another_strategy_does_not_fold(
    monkeypatch, fake_providers
):
    async def fake_sys(self_id, kernel):
        return "s" * 4000

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    fake_providers["ai"] = _FoldingModel()
    k = _K()
    await _long_head(k)
    r = await _context_recall("ai", {"dry": True}, k, _cfg())
    assert r["folded"] == 0 and not k.folds_of(), (
        "dry makes no model call, so no summary and no fold"
    )
    k2 = _K()
    await _long_head(k2, strategy="truncate")
    r2 = await _context_recall("ai", {}, k2, _cfg())
    assert r2["folded"] == 0 and not k2.folds_of()
    assert k.emitted == [] and k2.emitted == []


async def test_a_head_that_fits_is_not_folded(monkeypatch, fake_providers):
    async def fake_sys(self_id, kernel):
        return "s"

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    fake_providers["ai"] = model = _FoldingModel()
    k = _K()
    k.record["context_strategy"] = "fold"
    await _seed(k, [_row("user", "GOAL", "user:goal")] + _chain("a", 3))
    r = await _context_recall("ai", {}, k, _cfg(), client_id="c1")
    assert r["folded"] == 0 and model.summaries == 0 and not k.folds_of()
    assert [p for _, p in k.emitted if p.get("type") == "context"] == []


# ── recall pins the standing rows (#218, Rust #318) ───────────────────

DRIVER_TASK = "You are the DRIVER. ROLE-MARKER-218: keep the workers busy."


def test_standing_pins_the_task_the_live_root_and_the_newest_fold():
    def r(i, role, content, parent=0, **kw):
        return {"id": i, "role": role, "content": content, "parent": parent, **kw}

    view = [
        r(0, "user", "[cleared]", kind="clear"),
        r(1, "user", "TASK"),
        r(2, "assistant", "ok", 1),
        r(3, "user", "[reply to 9 · exec status]\nrunning", 2),
        r(4, "user", "[from w1] hi"),
        r(5, "user", "folded", 0, kind="fold", refs=[2, 3]),
        r(6, "user", "later note"),
        r(7, "assistant", "ok", 6),
    ]
    out = _standing(view)
    assert out == [1, 6, 5, 4], out
    assert len(set(out)) == len(out) and 0 not in out
    assert _standing([r(0, "user", "goal"), r(1, "assistant", "a", 0)]) == []
    assert _standing([r(0, "user", "goal"), r(1, "user", "later")]) == [1]
    assert (
        _standing([r(0, "user", "goal"), r(1, "user", "[reply to 9 · exec status]\nx")])
        == []
    )
    assert not _is_task_root(r(1, "user", "[reply to 9 · exec status]\nx"))
    assert not _is_task_root(r(1, "user", "x", 5))
    assert not _is_task_root(r(1, "user", "x", kind="recall"))
    assert not _is_task_root(r(1, "assistant", "x"))
    assert _is_task_root(r(1, "user", "x"))
    assert _is_task_root(r(1, "user", "x", kind=""))
    assert _is_task_root(r(1, "user", "x", kind="user"))
    assert _is_task_root({"id": 1, "role": "user", "content": "x", "kind": None})


async def _driver_store(k, clear_first, task_text):
    if clear_first:
        await _seed(
            k,
            [
                _row("user", "an old job", "user:old"),
                _row("assistant", "done with the old job", "ai:old", "user:old"),
            ],
        )
        await core._context_clear("ai", {"why": "new job"}, k)
        rows = []
    else:
        rows = [
            _row("user", "hello", "user:hello"),
            _row("assistant", "hi", "ai:hello", "user:hello"),
        ]
    rows.append(_row("user", task_text, "user:task"))
    rows.append(_row("assistant", "Understood.", "ai:ack", "user:task"))
    filler = "w" * 1200
    for i in range(60):
        a = _row(
            "assistant",
            f'Polling job {i}. {filler} <call name="send"><target_id>exec</target_id>'
            f"<title>poll {i}</title><type>status</type></call>",
            f"ai:poll{i}",
            "ai:ack",
        )
        rows.append(a)
        rows.append(
            _row(
                "user",
                f'[reply to {a["id"]} · exec status · "poll {i}"]\nrunning',
                f"user:reply{i}",
                a["id"],
            )
        )
        if i % 10 == 9:
            rows.append(_row("user", f"[from worker_1 · msg {i}] done", f"user:w{i}"))
    await _seed(k, rows)


class _Recorder:
    last_stop_reason = None

    def __init__(self):
        self.requests = []

    async def chat(self, messages, max_tokens=None):
        self.requests.append(json.dumps(messages, default=str))
        yield '<call name="done"></call>'


async def _restarted_turn_sees(monkeypatch, fake_providers, clear_first, task_text):
    async def fake_sys(self_id, kernel):
        return "sys"

    monkeypatch.setattr(core, "_default_system_content", fake_sys)
    k = _K()
    await _driver_store(k, clear_first, task_text)
    await _make_boot(_cfg())("ai", {}, k)  # the restart: recall due, head cache gone
    fake_providers["ai"] = rec = _Recorder()
    await core._run("ai", "start ais", k, "c218", _cfg())
    turn = [q for q in rec.requests if "Choose which rows you want" not in q]
    assert turn, rec.requests
    return k, turn[0]


async def test_a_restarted_agent_still_sees_the_turn_that_set_its_task(
    monkeypatch, fake_providers
):
    _, text = await _restarted_turn_sees(monkeypatch, fake_providers, False, DRIVER_TASK)
    assert "ROLE-MARKER-218" in text


async def test_a_restart_after_a_clear_keeps_the_task_given_after_it(
    monkeypatch, fake_providers
):
    _, text = await _restarted_turn_sees(monkeypatch, fake_providers, True, DRIVER_TASK)
    assert "ROLE-MARKER-218" in text
    assert "an old job" not in text and "done with the old job" not in text


async def test_pinning_stays_within_half_the_target_and_newest_rows_still_fill(
    monkeypatch, fake_providers
):
    _, text = await _restarted_turn_sees(
        monkeypatch, fake_providers, False, "GIANT-218 " + "y" * 1_000_000
    )
    assert "GIANT-218" not in text, "an oversized pin is skipped"
    assert "Polling job 59" in text, "the newest rows still fill"

    # direct: a mid-sized pin is taken, and the pins stay within PIN_SHARE of target
    k = _K()
    await _driver_store(k, False, "MID-218 " + "z" * 3000)
    await _context_recall("ai", {"dry": True}, k)
    v = await _load_view("ai", k)
    rows = {r["id"]: r for r in await core._load_history("ai", k)}
    ids = [e["id"] for e in v["ids"]]
    sys_tokens = estimate_one(
        {"role": "system", "content": await core._system_content("ai", k, None)}
    )
    body = max(
        core.BUDGET_FLOOR,
        core.budget(k.record) - sys_tokens - core.NOTICE_ENVELOPE_RESERVE,
    )
    from ai_core.strategies.strip import LOW

    target = int(body * LOW)
    mid = [i for i in ids if "MID-218" in str(rows[i].get("content", ""))]
    assert mid, "the mid-sized task is pinned"
    cost = sum(
        estimate_one({"role": "user", "content": rows[i]["content"]}) for i in mid
    )
    assert cost <= int(target * core.PIN_SHARE)
