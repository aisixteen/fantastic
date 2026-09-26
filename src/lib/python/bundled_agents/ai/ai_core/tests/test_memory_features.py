"""The memory-store era of ai_core: calibrated compaction trigger, repeat-read dedupe,
fold summaries with a budget and a mechanical index, and context by grant
(`fork_from`, `request_share`)."""

from __future__ import annotations

import pytest

from ai_core import core
from ai_core.context import ProjectionCtx
import importlib

from _fake import FakeCtx
from _testkit import memory_meta
from kernel import sender_context

fold_mod = importlib.import_module("ai_core.strategies.fold")


# ── the trigger, calibrated by provider-reported usage ─────────────────


class _Reporting:
    """A provider that REPORTS a prompt size, like the real endpoints do."""

    def __init__(self, reported: int):
        self.reported = reported
        self.last_prompt_tokens = None

    async def chat(self, messages, max_tokens=None):
        self.last_prompt_tokens = self.reported
        yield 'ok <call name="done"></call>'


async def test_the_endpoints_count_calibrates_the_estimate():
    k = FakeCtx({"context_window": 12000, "output_reserve": 0})
    msgs = [{"role": "system", "content": "s"}, {"role": "user", "content": "x" * 4000}]
    core._calib.pop("ai", None)
    # under the trigger by our own estimate (~1000 of a ~12k budget)
    out = await core._project_context(k.record, _cfg(), None, "ai", k, "cli", msgs, 1)
    assert out == msgs and core._projection["ai"]["fired"] is False
    # the endpoint said the same prompt was TEN times bigger — the budget shrinks
    core._calib["ai"] = 10.0
    out = await core._project_context(k.record, _cfg(), None, "ai", k, "cli", msgs, 1)
    assert isinstance(out, list) and core._projection["ai"]["fired"] is True
    assert core._projection["ai"]["calibration"] == 10.0
    core._calib.pop("ai", None)


async def test_a_turn_records_what_the_endpoint_reported():
    k = FakeCtx({"context_window": 32000, "output_reserve": 0})
    core._providers["ai"] = _Reporting(777)
    try:
        await core._run("ai", "hello", k, "cli", _cfg())
    finally:
        core._providers.pop("ai", None)
    assert core._reported["ai"] == 777 and core._calib["ai"] > 0
    st = await core._context_status("ai", {}, k)
    assert st["reported_prompt_tokens"] == 777 and st["trigger"] == 0.8
    core._calib.pop("ai", None)
    core._reported.pop("ai", None)


def _cfg():
    return core.Backend(
        sentence="s",
        default_model="m",
        default_endpoint="e",
        make_provider=lambda *a: None,
        name="fake",
        module_name="fake.tools",
    )


# ── the same bytes twice: one copy in view ─────────────────────────────


def test_a_repeated_body_is_a_stub_citing_the_first():
    body = "line\n" * 200
    hist = [
        {"role": "user", "id": 1, "content": f"[reply to 10 · notes read]\n{body}"},
        {"role": "assistant", "id": 2, "content": "again"},
        {"role": "user", "id": 3, "content": f"[reply to 11 · notes read]\n{body}"},
        {"role": "user", "id": 4, "content": "[reply to 12 · notes read]\nsmall"},
    ]
    out = core.dedupe_repeat_bodies(hist)
    assert out[0] == hist[0], "the first copy stays whole"
    assert "identical to the reply in row 1" in out[2]["content"]
    assert out[2]["id"] == 3 and out[3] == hist[3]


# ── the fold summary: a budget, one retry, then refuse ─────────────────


def _rows(n=6, size=2000):
    rows = [{"role": "user", "id": 1, "content": "the goal: write report.md"}]
    for i in range(n):
        rows.append({"role": "assistant", "id": 10 + i, "content": "x" * size})
    return rows


async def test_an_oversized_summary_is_resummarised_once_then_refused():
    asks: list[str] = []

    async def verbose(msgs, max_tokens=None):
        asks.append(msgs[0]["content"])
        return "TITLE\n\n" + "word " * 5000

    ctx = ProjectionCtx(
        budget=4000,
        recent_n=4,
        summarize=verbose,
        self_id="ai",
        kernel=None,
        client_id=None,
    )
    rows = _rows()
    out = await fold_mod._summarize(ctx, rows, list(range(1, 6)))
    assert out is None, "still over budget after the retry — nothing is folded"
    assert len(asks) == 2 and "Rewrite it to fit" in asks[1]
    assert "AT MOST ~" in asks[0] and "ERRORS:" in asks[0], "fixed sections, a target"


async def test_a_retry_that_fits_is_the_summary():
    calls = []

    async def shrinks(msgs, max_tokens=None):
        calls.append(1)
        return "TITLE\n\n" + ("word " * 5000 if len(calls) == 1 else "short summary")

    ctx = ProjectionCtx(
        budget=4000,
        recent_n=4,
        summarize=shrinks,
        self_id="ai",
        kernel=None,
        client_id=None,
    )
    title, body, degraded = await fold_mod._summarize(ctx, _rows(), list(range(1, 6)))
    assert title == "TITLE" and body == "short summary" and not degraded


def test_the_mechanical_index_keeps_files_ids_and_the_users_words():
    rows = [
        {
            "role": "user",
            "id": 1,
            "content": "please check filler_3.txt for deadbeef99",
        },
        {"role": "assistant", "id": 2, "content": "reading notes/a.md"},
        {"role": "user", "id": 3, "content": "[reply to 9 · notes read]\nbody"},
    ]
    idx = fold_mod.mechanical_index(rows, [0, 1, 2])
    assert "filler_3.txt" in idx and "notes/a.md" in idx
    assert "deadbeef99" in idx
    assert "user said: please check filler_3.txt for deadbeef99" in idx
    assert "[reply to" not in idx, "runtime lines are not the user's words"


# ── context by grant: fork_from, request_share ─────────────────────────


async def _pair(kernel):
    import ollama_backend.tools  # noqa: F401

    for aid, peer in (("alice", "bob"), ("bob", "alice")):
        kernel.create(
            "ollama_backend.tools",
            id=aid,
            callable_by={peer: "*"},
            **memory_meta(kernel, aid, {"cli": "*", peer: "*"}),
        )


async def test_fork_from_reads_the_owners_rows_under_the_grant(seeded_kernel):
    await _pair(seeded_kernel)
    await core._save_history(
        "alice", seeded_kernel, [{"role": "user", "content": "the vault code is 4711"}]
    )
    with sender_context("alice"):
        g = (
            await seeded_kernel.send(
                [
                    {
                        "target": "memory",
                        "payload": {
                            "type": "share",
                            "with": "bob",
                            "scope": {"head_until": 10**9},
                        },
                    }
                ]
            )
        )[0]["result"]
    delivered: list = []
    real = core._deliver_soon
    core._deliver_soon = lambda k, t, text, c, s: delivered.append((t, text))
    try:
        out = await core._fork_from(
            "bob",
            {"owner": "alice", "grant": g["grant"], "message": "what is the code?"},
            seeded_kernel,
        )
    finally:
        core._deliver_soon = real
    assert out["forked"] and out["rows"] == 1
    hist = await core._load_history("bob", seeded_kernel)
    fork = next(r for r in hist if r.get("kind") == "fork")
    assert "[shared from alice · read-only" in fork["content"]
    assert "4711" in fork["content"], "the owner's row, read back under the grant"
    assert delivered and delivered[0][0] == "bob"
    assert "you are a new agent, not alice" in delivered[0][1]
    assert delivered[0][1].endswith("what is the code?"), "the kickstart message"
    # the owner revokes — the reader's next read fails fast
    with sender_context("alice"):
        await seeded_kernel.send(
            [{"target": "memory", "payload": {"type": "unshare", "grant": g["grant"]}}]
        )
    with pytest.raises(core.MemoryStoreError):
        await core._load_history("bob", seeded_kernel)


async def test_request_share_reaches_the_owners_model(seeded_kernel):
    await _pair(seeded_kernel)
    delivered: list = []
    real = core._deliver_soon
    core._deliver_soon = lambda k, t, text, c, s: delivered.append((t, text))
    try:
        with sender_context("bob"):
            out = (
                await seeded_kernel.send(
                    [
                        {
                            "target": "alice",
                            "payload": {
                                "type": "request_share",
                                "row": 42,
                                "why": "need it",
                            },
                        }
                    ]
                )
            )[0]["result"]
    finally:
        core._deliver_soon = real
    assert out == {"requested": True, "owner": "alice", "row": 42}
    assert delivered[0][0] == "alice"
    assert "[request_share from bob · row 42] need it" in delivered[0][1]
    assert "<type>share</type>" in delivered[0][1]


async def test_a_shared_scope_shows_a_third_owners_row_as_a_stub():
    rows = [
        {"id": 1, "role": "user", "content": "mine"},
        {
            "id": 2,
            "role": "user",
            "kind": "fork",
            "owner": "carol",
            "as_of": 57,
            "content": "[fork of carol]",
        },
    ]
    text = core._render_shared("alice", 2, rows)
    assert "(user · row 1) mine" in text
    # carol's OWN row id — the one `request_share` to carol can name
    assert "[row 57 · owner carol · not shared with you]" in text


# ── the head cache never diverges from the store ───────────────────────


async def test_a_row_another_writer_appends_reaches_the_next_head_read(seeded_kernel):
    """The head is cached write-through, but the store is the truth: a row an admin
    appends under the agent's key after the cache is warm is in the next read."""
    await _pair(seeded_kernel)
    await core._save_history(
        "alice", seeded_kernel, [{"role": "user", "content": "first"}]
    )
    assert [r["content"] for r in await core._load_history("alice", seeded_kernel)] == [
        "first"
    ]
    rid = seeded_kernel.ctx.mint_id()
    out = await seeded_kernel.send(
        [
            {
                "target": "memory",
                "payload": {
                    "type": "append",
                    "owner": "alice",
                    "rows": [{"id": rid, "role": "user", "content": "planted"}],
                },
            }
        ]
    )
    assert out[0]["result"].get("ids") == [rid], out
    hist = await core._load_history("alice", seeded_kernel)
    assert [r["content"] for r in hist] == ["first", "planted"]
    # and read again: merged once, not twice
    assert len(await core._load_history("alice", seeded_kernel)) == 2


async def test_a_note_the_agent_appends_itself_is_memory_not_a_turn(seeded_kernel):
    """A row without a chat `role` (the agent's own note, appended straight to its
    store) stays in the store — never rendered as a message."""
    await _pair(seeded_kernel)
    await core._save_history(
        "alice", seeded_kernel, [{"role": "user", "content": "hi"}]
    )
    await core._load_history("alice", seeded_kernel)  # the cache is warm
    with sender_context("alice"):
        out = await seeded_kernel.send(
            [
                {
                    "target": "memory",
                    "payload": {
                        "type": "append",
                        "rows": [{"kind": "note", "content": "vault code 4711"}],
                    },
                }
            ]
        )
    assert out[0]["result"].get("appended") == 1, out
    hist = await core._load_history("alice", seeded_kernel)
    assert [r["content"] for r in hist] == ["hi"]
    behind = await core._load_history("alice", seeded_kernel, behind=True)
    assert all(r.get("role") for r in behind)


async def test_a_turns_rows_are_minted_when_appended_so_id_order_is_append_order():
    """A row saved MID-TURN (the projection's fold or view) must not end up behind the
    turn's own rows in id order: the store returns id order, the cache append order."""
    k = FakeCtx({"context_window": 32000, "output_reserve": 0})

    class _MidTurnWriter:
        last_prompt_tokens = None

        async def chat(self, messages, max_tokens=None):
            await core._save_history("ai", k, [{"role": "user", "content": "mid"}])
            yield 'ok <call name="done"></call>'

    core._providers["ai"] = _MidTurnWriter()
    try:
        await core._run("ai", "hello", k, "cli", _cfg())
    finally:
        core._providers.pop("ai", None)
    hist = await core._load_history("ai", k)
    ids = [r["id"] for r in hist]
    assert ids == sorted(ids), f"append order diverged from id order: {ids}"
    assert [r["content"] for r in hist][:2] == ["mid", "hello"]


async def test_fork_from_refuses_an_owner_the_grant_does_not_name(seeded_kernel):
    """The attribution label is the grant's owner, never the caller's word."""
    await _pair(seeded_kernel)
    await core._save_history(
        "alice", seeded_kernel, [{"role": "user", "content": "alice's row"}]
    )
    with sender_context("alice"):
        g = (
            await seeded_kernel.send(
                [
                    {
                        "target": "memory",
                        "payload": {
                            "type": "share",
                            "with": "bob",
                            "scope": {"head_until": 10**9},
                        },
                    }
                ]
            )
        )[0]["result"]
    out = await core._fork_from(
        "bob",
        {"owner": "mallory", "grant": g["grant"], "message": "go"},
        seeded_kernel,
    )
    assert out.get("reason") == "bad_arg" and "given by alice" in out["error"]
    out = await core._fork_from(
        "bob", {"owner": "alice", "grant": 99999, "message": "go"}, seeded_kernel
    )
    assert out.get("reason") == "not_found"


class _CutThenFit:
    """A summarizer whose provider stops AT the bound the first time."""

    def __init__(self, cuts: int):
        self.cuts, self.asked, self.last_cut = cuts, [], False

    async def __call__(self, msgs, max_tokens=None):
        self.asked.append(max_tokens)
        self.last_cut = len(self.asked) <= self.cuts
        return "TITLE\n\n" + ("partial" if self.last_cut else "whole summary")


async def test_a_summary_cut_at_its_budget_is_resummarised_once():
    s = _CutThenFit(cuts=1)
    ctx = ProjectionCtx(
        budget=4000,
        recent_n=4,
        summarize=s,
        self_id="ai",
        kernel=None,
        client_id=None,
    )
    rows = _rows()
    title, body, _ = await fold_mod._summarize(ctx, rows, list(range(1, 6)))
    assert body == "whole summary" and len(s.asked) == 2
    want = fold_mod.summary_budget(ctx, rows, list(range(1, 6)))
    bound = want + fold_mod.THINK_ALLOWANCE  # the thought counts against max_tokens
    assert s.asked == [bound, bound], "the call is bounded by budget + thinking"


async def test_a_summary_cut_twice_is_refused_nothing_folded():
    s = _CutThenFit(cuts=2)
    ctx = ProjectionCtx(
        budget=4000,
        recent_n=4,
        summarize=s,
        self_id="ai",
        kernel=None,
        client_id=None,
    )
    assert await fold_mod._summarize(ctx, _rows(), list(range(1, 6))) is None
