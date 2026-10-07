"""Deterministic, no-LLM tests for the context-budget primitives, the overflow
strategies (the projection logic), and the Context Protocol seam + verbs (the unified
notice, the `context` events, `recall`, and the derived `last_reaction`). Fast; no
network. The seam/verb tests use a tiny fake kernel + monkeypatched `_load_history`."""

from __future__ import annotations

import json
import types

import pytest

from ai_core import core
from ai_core.context import (
    DEFAULT_CONTEXT_WINDOW,
    DEFAULT_OUTPUT_RESERVE,
    ProjectionCtx,
    budget,
    estimate_one,
    estimate_tokens,
    output_reserve,
    resolve_context_window,
)
from ai_core.core import _compaction_mark, _project_context, _projection
from ai_core.strategies import DEFAULT_STRATEGY, get_strategy, strategy_names
from ai_core.strategies.base import STUB_SUMMARY, Projection

from _fake import FakeCtx  # noqa: E402

_NoLegKernel = FakeCtx
_FakeKernel = FakeCtx


def _turn(role: str, content: str) -> dict:
    return {"role": role, "content": content}


def _body(n: int, size: int = 200) -> list[dict]:
    return [
        _turn("user" if i % 2 == 0 else "assistant", f"turn{i}-" + "x" * size)
        for i in range(n)
    ]


async def _fake_summary(_msgs, max_tokens=None) -> str:
    return "SUMMARY"


def _ctx(
    bud: int, recent_n: int = 4, summarize=_fake_summary, kernel=None
) -> ProjectionCtx:
    return ProjectionCtx(
        budget=bud,
        recent_n=recent_n,
        summarize=summarize,
        self_id="ai",
        kernel=kernel if kernel is not None else _NoLegKernel(),
    )


class _FakeProvider:
    async def chat(self, messages, max_tokens=None):
        yield "FAKESUMMARY"


def _cfg():
    return types.SimpleNamespace(name="test_backend")


def _msgs(n):
    return [_turn("system", "sys")] + _body(n)


# ── primitives ──────────────────────────────────────────────────────


def test_estimate_scale_and_monotonic():
    assert estimate_one(_turn("user", "x" * 400)) >= 100  # ~400/4
    assert estimate_tokens(_body(4)) > estimate_tokens(_body(2))


def test_window_precedence():
    assert resolve_context_window({"context_window": 8000, "num_ctx": 2000}) == 8000
    assert resolve_context_window({"num_ctx": 2000}) == 2000
    assert resolve_context_window({}) == DEFAULT_CONTEXT_WINDOW


def test_budget_floor():
    assert budget({"context_window": 100, "output_reserve": 1000}) == 256


@pytest.mark.parametrize(
    "v, want",
    [
        ("٣", None),
        ("²", None),
        ("12٣", None),
        (" 3 ", 3),
        (" 3 ", 3),
        ("\x1c3", 3),
        ("0", 0),
        ("+5", None),
        ("", None),
        (7, 7),
        (0, None),
        (True, None),
        (2.0, None),
    ],
)
def test_id_digit_strings_are_ascii_only(v, want):
    # rust tests.rs `id_and_window_digit_strings_are_ascii_only`, same vectors.
    assert core._as_id(v) == want


@pytest.mark.parametrize(
    "v, window, reserve",
    [
        ("٣", DEFAULT_CONTEXT_WINDOW, DEFAULT_OUTPUT_RESERVE),
        ("²", DEFAULT_CONTEXT_WINDOW, DEFAULT_OUTPUT_RESERVE),
        ("  12 ", 12, 12),
        # `str.strip` drops the separators `\x1c`-`\x1f`, `int()` does not: the
        # stripped text is what gets parsed, so these are numbers, not a raise.
        ("\x1c12", 12, 12),
        ("\x1f12\x1d", 12, 12),
        ("0", DEFAULT_CONTEXT_WINDOW, 0),
        ("\x1c", DEFAULT_CONTEXT_WINDOW, DEFAULT_OUTPUT_RESERVE),
        ("+5", DEFAULT_CONTEXT_WINDOW, DEFAULT_OUTPUT_RESERVE),
    ],
)
def test_window_and_reserve_digit_strings_parse_what_they_check(v, window, reserve):
    rec = {"context_window": v, "output_reserve": v}
    assert resolve_context_window(rec) == window
    assert output_reserve(rec) == reserve


def test_registry_no_memgpt():
    # memgpt collapsed into the universal seam notice — it is no longer a strategy.
    assert set(strategy_names()) == {"compact", "truncate", "strip", "fold"}
    assert DEFAULT_STRATEGY == "compact"
    assert get_strategy("memgpt") is None
    assert get_strategy("nope") is None  # unknown ⇒ None (caller errors), no default


# ── strategies (now return a Projection, NOT a fabricated turn) ──────


async def test_compact_returns_projection_with_summary():
    body = _body(10)
    proj = await get_strategy("compact")(
        body, [_turn("system", "S")], {}, _ctx(bud=1_000_000, recent_n=4)
    )
    assert isinstance(proj, Projection)
    assert proj.summary == "SUMMARY"  # artifact, not a fabricated turn
    # The task-framing turn is PINNED ahead of the recent window (see
    # `test_every_strategy_keeps_the_goal_when_the_body_overflows` for why: the goal
    # is the oldest turn, so the overflow split took it first and the agent lost its
    # objective mid-run). Everything after it is the recent window, verbatim.
    assert proj.body == [body[0]] + body[-4:]
    # the strategy never fabricates the old leading marker turn
    assert not any("[Earlier conversation summary]" in m["content"] for m in proj.body)


async def test_truncate_returns_projection_no_summarizer():
    called = []

    async def spy(_msgs, max_tokens=None):
        called.append(1)
        return "S"

    body = _body(10)
    proj = await get_strategy("truncate")(
        body, [], {}, _ctx(bud=1_000_000, recent_n=4, summarize=spy)
    )
    assert isinstance(proj, Projection)
    assert proj.omitted_marker is True and proj.summary is None
    assert proj.body[0] == body[0]  # first (task) turn kept
    assert proj.body[-4:] == body[-4:]  # recent kept
    assert not any("[…" in m["content"] for m in proj.body)  # no fabricated marker turn
    assert not called, "truncate must NOT call the summarizer"


async def test_summarizer_failure_degrades_to_stub():
    async def boom(_msgs, max_tokens=None):
        raise RuntimeError("nope")

    proj = await get_strategy("compact")(
        _body(10), [], {}, _ctx(bud=1_000_000, summarize=boom)
    )
    assert proj.summary == STUB_SUMMARY


async def test_tight_budget_keeps_live_user_turn():
    body = _body(10)
    proj = await get_strategy("truncate")(body, [], {}, _ctx(bud=80, recent_n=4))
    assert proj.body[-1] == body[-1]


# ── the unified context-notice ──────────────────────────────────────


def test_context_notice_compact_shape():
    n = core._context_notice("compact", "MY SUMMARY", False, 7)
    assert n["role"] == "user"
    c = n["content"]
    assert "[context-notice]" in c and "MY SUMMARY" in c
    assert "recall" in c and "memory agent" in c  # affordances taught
    assert "7 earlier turn" in c


def test_context_notice_truncate_shape():
    n = core._context_notice("truncate", None, True, 3)
    c = n["content"]
    assert "omitted in place" in c and "recall" in c
    assert "Summary of the dropped span" not in c


# ── the projection seam (deterministic, with a fake kernel) ─────────


async def test_seam_short_circuits_when_under_budget():
    msgs = [_turn("system", "S"), _turn("user", "hi")]
    k = _FakeKernel()
    out = await _project_context(
        {}, _cfg(), _FakeProvider(), "ai", k, "cli", msgs, len(msgs) - 1
    )
    assert out == msgs  # unchanged
    assert _projection["ai"] == {"fired": False, "trigger": 0.8, "calibration": 1.0}
    assert k.sent == [] and k.emitted == []  # no context event when nothing compacts


async def test_seam_fires_prepends_notice_and_emits_event():
    msgs = _msgs(20)  # big history
    k = _FakeKernel()
    out = await _project_context(
        {"context_window": 500, "output_reserve": 0, "context_strategy": "compact"},
        _cfg(),
        _FakeProvider(),
        "ai",
        k,
        "cli",
        msgs,
        len(msgs) - 1,
    )
    assert estimate_tokens(out) <= 500  # fits the window now
    assert out[0]["role"] == "system"  # system block preserved
    assert (
        out[1]["role"] == "user" and "[context-notice]" in out[1]["content"]
    )  # notice
    proj = _projection["ai"]
    assert proj["fired"] is True and proj["strategy"] == "compact"
    assert proj["summarized"] is True and proj["dropped_messages"] > 0
    # public projection record stays a clean summary — no internal scan bookkeeping
    assert "fired_at_index" not in proj and "client_id" not in proj
    # the private reaction cursor holds the bookkeeping instead
    assert "fired_at_index" in _compaction_mark["ai"]
    # the context:compacted event reached the caller (cli → kernel.send)
    ctx_events = [p for t, p in k.sent if p.get("type") == "context"]
    assert len(ctx_events) == 1
    ev = ctx_events[0]
    assert ev["phase"] == "compacted" and ev["detail"]["strategy"] == "compact"
    assert ev["detail"]["dropped_messages"] > 0


async def test_seam_notice_is_not_in_the_input_history():
    # The notice is a SEAM artifact — it must not appear in the conversation we projected
    # FROM (it is added only to the model view, never the durable record).
    msgs = _msgs(20)
    assert "[context-notice]" not in json.dumps(msgs)
    k = _FakeKernel()
    await _project_context(
        {"context_window": 500, "output_reserve": 0, "context_strategy": "compact"},
        _cfg(),
        _FakeProvider(),
        "ai",
        k,
        "web1",
        msgs,
        len(msgs) - 1,
    )
    # the input list itself was not mutated to contain the notice
    assert "[context-notice]" not in json.dumps(msgs)


async def test_seam_failsafe_too_small_emits_event_and_errors():
    big_system = _turn("system", "S" * 8000)  # ~2000 tok, bigger than the window
    msgs = [big_system, _turn("user", "hi")]
    k = _FakeKernel()
    out = await _project_context(
        {"context_window": 1000, "output_reserve": 0},
        _cfg(),
        _FakeProvider(),
        "ai",
        k,
        "cli",
        msgs,
        len(msgs) - 1,
    )
    assert isinstance(out, dict) and "context_insufficient" in out["error"]
    assert _projection["ai"].get("too_small") is True
    ev = [p for t, p in k.sent if p.get("type") == "context"]
    assert len(ev) == 1 and ev[0]["phase"] == "too_small"
    assert "hint" in ev[0]["detail"]


async def test_seam_unknown_strategy_errors_without_event():
    msgs = _msgs(20)
    k = _FakeKernel()
    out = await _project_context(
        {"context_window": 500, "output_reserve": 0, "context_strategy": "bogus"},
        _cfg(),
        _FakeProvider(),
        "ai",
        k,
        "cli",
        msgs,
        len(msgs) - 1,
    )
    assert isinstance(out, dict) and "bogus" in out["error"]
    assert [p for t, p in k.sent if p.get("type") == "context"] == []


# ── recall verb (read-only page-back over the durable store) ────────


def _send_turn(target, payload):
    """An assistant turn carrying a `<tool_call>` envelope inline — the shape a
    mind (or an old transcript) leaves in the durable store for `_derive_reaction`
    to detect. Built inline (the backend no longer renders tool calls)."""
    from ai_core.tool_parse import render_call

    return {
        "role": "assistant",
        "content": render_call("send", {"target_id": target, "payload": payload}),
    }


async def test_recall_filters_paginates_and_caps(monkeypatch):
    store = [
        _turn("user", "the project codename is HALCYON"),
        _turn("assistant", "noted"),
        _turn("user", "tell me about bicycles"),
        _turn("assistant", "x" * 5000),  # bulky — must be capped
        _turn("user", "what about codename again"),
    ]

    async def fake_load(self_id, kernel, behind=False):
        return store

    monkeypatch.setattr(core, "_load_history", fake_load)

    # substring query (case-insensitive) finds both codename turns
    r = await core._recall("ai", {"query": "codename"}, None)
    assert r["total"] == 2
    assert all("codename" in m["content"].lower() for m in r["messages"])
    assert r["messages"][0]["index"] == 0  # store index preserved

    # limit + truncated flag
    r2 = await core._recall("ai", {"limit": 1}, None)
    assert len(r2["messages"]) == 1 and r2["truncated"] is True

    # before paging (only turns with index < 2)
    r3 = await core._recall("ai", {"before": 2}, None)
    assert {m["index"] for m in r3["messages"]} == {0, 1}

    # A recalled turn comes back WHOLE. It was capped at 2000 chars "so paging back
    # cannot blow the window" — but recall exists to hand the model a detail it no
    # longer has, and a capped turn silently drops the part it went looking for.
    # `limit` is the bound that belongs here: the caller says how many turns it
    # wants, and each one arrives intact.
    rbig = await core._recall("ai", {"query": "xxxx"}, None)
    assert len(rbig["messages"][0]["content"]) == 5000, "recall returns the turn whole"


async def test_recall_empty_page_says_why(monkeypatch):
    """An empty page carries a `note` naming the reason — the bare `{total: 0}` was
    read by a live model as "there is no durable transcript" when the truth was
    "nothing has been compacted"."""
    store: list[dict] = []

    async def fake_load(self_id, kernel, behind=False):
        return store

    monkeypatch.setattr(core, "_load_history", fake_load)
    _projection.pop("ai", None)
    # no compaction ever fired ⇒ the note says the live context is complete
    r = await core._recall("ai", {}, None)
    assert r["total"] == 0 and "compacted" in r["note"]
    # query matched nothing in a NON-empty store ⇒ the note says that instead
    store.append(_turn("user", "hello"))
    rq = await core._recall("ai", {"query": "zzz"}, None)
    assert rq["total"] == 0 and "matched none" in rq["note"]


# ── context_status.last_reaction (derived 'ack') ────────────────────


async def test_derive_reaction_none_before_any_compaction(monkeypatch):
    _projection.pop("ai", None)
    assert await core._derive_reaction("ai", None) is None
    _projection["ai"] = {"fired": False}
    assert await core._derive_reaction("ai", None) is None


async def test_derive_reaction_detects_recall_and_persist(monkeypatch):
    store = [
        _turn("user", "old turn"),  # index 0 (before the notice)
        _turn("user", "what's my codename?"),  # index 1 — this turn (fired_at_index)
        _send_turn(
            "ai", {"type": "recall", "query": "codename"}
        ),  # the recall reaction
        # the persist reaction — a file_bridge `write`
        _send_turn("notes", {"type": "write", "path": "facts.md", "text": "HALCYON"}),
        _turn("assistant", "It's HALCYON."),
    ]

    async def fake_load(self_id, kernel, behind=False):
        return store

    monkeypatch.setattr(core, "_load_history", fake_load)
    _projection["ai"] = {"fired": True}
    _compaction_mark["ai"] = {"fired_at_index": 1, "client_id": "cli"}
    r = await core._derive_reaction("ai", None)
    assert r == {"recalled": True, "persisted": True, "recall_count": 1}


async def test_reaction_counts_the_file_bridge_text_verbs(monkeypatch):
    """The persist verbs are a file_bridge's `write`/`edit`/`append` — a model
    keeps what it must as plain files; any other verb is not a persist."""
    thread: list[dict] = []

    async def fake_load(self_id, kernel, behind=False):
        return thread

    monkeypatch.setattr(core, "_load_history", fake_load)
    _projection["ai"] = {"fired": True}
    _compaction_mark["ai"] = {"fired_at_index": 0, "client_id": "cli"}
    try:
        for verb in ("write", "edit", "append"):
            thread[:] = [_send_turn("notes", {"type": verb, "path": "facts.md"})]
            r = await core._derive_reaction("ai", None)
            assert r["persisted"] is True, verb
        for other in ("read", "list"):
            thread[:] = [_send_turn("notes", {"type": other, "path": "facts.md"})]
            r = await core._derive_reaction("ai", None)
            assert r["persisted"] is False, other
    finally:
        _projection.pop("ai", None)
        _compaction_mark.pop("ai", None)


async def test_the_reaction_cursor_is_the_store_index_not_the_deduped_view(monkeypatch):
    """The mark is an index into the DURABLE STORE, never into the model view.

    `_derive_reaction` slices the full store by `fired_at_index`, but the index used
    to be `len(body) - 1` over the DEDUPED/trimmed view. Every turn `dedupe_stale_reads`
    dropped made the cursor that much too small, so the scan started BEFORE the notice
    and read an assistant turn written EARLIER as this compaction's reaction.

    The store below is exactly that shape: two `notes reflect` replies (the first
    superseded, so the view is one turn shorter than the store) and the memory write
    as the LAST stored turn — precisely where the old off-by-one cursor landed."""
    store = [
        _turn("user", "the ask: " + "x" * 1200),
        _turn("user", "[reply to ai:a1 · notes reflect]\n" + "A" * 1200),
        _turn("user", "[reply to ai:a2 · notes reflect]\n" + "B" * 1200),
        # written BEFORE the notice — not a reaction to a compaction that had not
        # yet fired. Nothing after this point in the store, so `persisted` is False.
        _send_turn("notes", {"type": "write", "path": "facts.md", "text": "HALCYON"}),
    ]

    async def fake_load(self_id, kernel, behind=False):
        return store

    monkeypatch.setattr(core, "_load_history", fake_load)
    k = _FakeKernel()
    messages, store_len = await core._assemble(
        "ai", "and now?", k, "cli", system_prompt="sys"
    )
    # the premise: the model view really IS shorter than the store.
    assert store_len == len(store) == 4
    assert len(messages) == 1 + (len(store) - 1) + 1, "system + deduped view + live"

    out = await _project_context(
        {"context_window": 500, "output_reserve": 0, "context_strategy": "compact"},
        _cfg(),
        _FakeProvider(),
        "ai",
        k,
        "cli",
        messages,
        store_len,
    )
    assert not isinstance(out, dict), out
    assert _projection["ai"]["fired"] is True
    # store index, not view index — 3 (the write turn) was the old, wrong answer.
    assert _compaction_mark["ai"]["fired_at_index"] == len(store) == 4
    r = await core._derive_reaction("ai", None)
    assert r == {"recalled": False, "persisted": False, "recall_count": 0}


# ── cli context render ──────────────────────────────────────────────


async def test_cli_context_renders_both_phases(capsys):
    import cli.tools as ct

    await ct._context(
        "cli",
        {
            "type": "context",
            "phase": "compacted",
            "source": "ai",
            "detail": {"strategy": "compact", "dropped_messages": 9},
        },
        None,
    )
    await ct._context(
        "cli",
        {
            "type": "context",
            "phase": "too_small",
            "source": "ai",
            "detail": {"hint": "raise context_window"},
        },
        None,
    )
    out = capsys.readouterr().out
    assert "compacted" in out and "compact" in out and "9 dropped" in out
    assert "too small" in out and "raise context_window" in out


# ─── the GOAL must survive every strategy ───────────────────────
#
# MEASURED FAILURE (2026-08-20, live the model on the `live_plan` probe, four runs).
# The task is stated ONCE, in the first user turn. On a long run the body overflows and
# `compact` — the DEFAULT — splits at `recent_n` and replaces the whole overflow with an
# LLM summary. The oldest turn IS the task framing, so the goal is the FIRST thing at
# risk. Observed end state: the model had already worked out the correct answer
# ("assets should be served to any browser client but is currently sealed"), then
# finished with "healthy budget, no tasks" and "Idle and ready. Give me a task
# whenever." — it did not refuse to act, it no longer knew what it was doing. Zero
# change-verbs issued across every run.
#
# `truncate` already gets this right ("keep the first (task-framing) turn"), which is
# what made the diagnosis obvious. This test is deliberately GENERAL over the strategy
# registry rather than written against one probe: any long task whose goal is stated
# once, up front, is exposed — which is most agent work. A new strategy inherits the
# guarantee by being registered.
async def test_every_strategy_keeps_the_goal_when_the_body_overflows():
    goal = "GOAL-SENTINEL: seal the secrets bridge and open the assets bridge."
    body = [_turn("user", goal)] + _body(40, size=400)

    for name in strategy_names():
        strategy = get_strategy(name)
        assert strategy is not None, name
        proj = await strategy(body, [], {}, _ctx(bud=1200, recent_n=4))
        # The projection is what the model actually sees. The goal must be reachable
        # from it — carried verbatim in the body, or preserved in the summary the seam
        # turns into the context notice. Anything else and the agent is flying blind.
        carried = any(goal in (m.get("content") or "") for m in proj.body)
        summarized = proj.summary is not None and goal in proj.summary
        assert carried or summarized, (
            f"strategy {name!r} dropped the task-framing turn: the agent loses its "
            f"goal mid-run (body={len(proj.body)} turns, summary={proj.summary!r})"
        )


# ─── reflect bulk: read once, then cut to its ids ───────────────
#
# MEASURED (2026-08-20, `live_plan`): reflect replies were 42% of the whole conversation.
# The body is kept for exactly one turn — the turn the model reads it — and once the
# model has answered, it is cut to a stub that KEEPS THE IDS it named (owner,
# 2026-10-06; before, the cut waited for a `SEEN` distillation and an undistilled
# reflect stayed whole: 13 full bodies, 36k chars, over ten e2e runs).
def test_a_reflect_reply_is_never_cut_from_the_view():
    """A reflect is MUTABLE state (owner, 2026-10-06): its body stays whole after
    the model answers it; only a newer reflect of the same agent supersedes it
    (`dedupe_stale_reads`). Only `docs` replies are cut."""
    tree = "<id>notes</id>\n<sentence>" + "x" * 4000 + "</sentence>"
    body = [
        _turn("user", "[reply to notes:aaa · notes reflect]\n" + tree),
        _turn(
            "assistant",
            '<call name="send"><target_id>notes</target_id><type>ps</type></call>',
        ),
        _turn("user", "next"),
    ]
    assert core.drop_read_docs(core.dedupe_stale_reads(body)) == body
    assert not hasattr(core, "drop_answered_reflects")


def test_stale_whole_state_reads_dedupe_but_parameterised_reads_never_do():
    body = [
        _turn("user", "[reply to ks:a1 · kernel_state reflect]\nOLD TREE"),
        _turn("assistant", "noted"),
        _turn("user", "[reply to fb:b1 · readings read]\nreading_1 = 41"),
        _turn("user", "[reply to fb:b2 · readings read]\nreading_2 = 42"),
        _turn("user", "[reply to ks:a2 · kernel_state reflect]\nNEW TREE"),
    ]
    out = core.dedupe_stale_reads(body)
    contents = [m["content"] for m in out]
    # whole-state: the newest reflect supersedes the older one
    assert not any("OLD TREE" in c for c in contents)
    assert any("NEW TREE" in c for c in contents)
    # parameterised: two `read`s are DIFFERENT FILES — dropping either loses data, and
    # the reply tag carries no args to tell them apart. Both must survive.
    assert any("reading_1 = 41" in c for c in contents)
    assert any("reading_2 = 42" in c for c in contents)


def test_a_combined_turn_trims_per_block_never_whole():
    """A batch arrives as ONE turn holding several `[reply to …]` blocks. The
    trimmer works per BLOCK: a superseded whole-state block dedupes alone, its
    sibling replies stay. Treating the turn as one reply either deleted siblings
    or trimmed nothing (a count guard skipped every multi-reply turn — tried
    2026-08-25, superseded same day)."""
    combined = (
        "[reply to ks:a1 · kernel_state reflect · 1 of 2 in batch ab12]\n"
        + "TREE" * 500
        + "\n\n[reply to tot:b1 · totals read · 2 of 2 in batch ab12]\n"
        + "reading_1 = 41"
    )
    body = [
        _turn("user", combined),
        _turn("assistant", "noted"),
        _turn("user", "[reply to ks:a2 · kernel_state reflect]\nNEW TREE"),
    ]
    out2 = core.dedupe_stale_reads(body)
    first = out2[0]["content"]
    assert "TREE" * 5 not in first, "older whole-state block is superseded"
    assert "reading_1 = 41" in first, "parameterised sibling must survive"
    assert any("NEW TREE" in m["content"] for m in out2)


def test_a_turn_whose_every_block_is_superseded_drops_entirely():
    body = [
        _turn(
            "user",
            "[reply to ks:a1 · kernel_state reflect · 1 of 2 in batch cd34]\nOLD"
            "\n\n[reply to k:a2 · kernel ps · 2 of 2 in batch cd34]\nOLD ROWS",
        ),
        _turn("assistant", "noted"),
        _turn("user", "[reply to ks:a3 · kernel_state reflect]\nNEW"),
        _turn("user", "[reply to k:a4 · kernel ps]\nNEW ROWS"),
    ]
    out = core.dedupe_stale_reads(body)
    contents = [m["content"] for m in out]
    assert not any("OLD" in c for c in contents)
    assert len(out) == 3, "the emptied turn leaves no husk behind"


# ── the SEAM must not undo what the strategies pinned ────────────────


@pytest.mark.parametrize("strategy", sorted(strategy_names()))
@pytest.mark.parametrize("window", [300, 400, 500, 700, 900, 1200])
async def test_the_seam_keeps_the_goal_the_strategies_pinned(strategy, window):
    """`test_every_strategy_keeps_the_goal_when_the_body_overflows` calls the
    strategies DIRECTLY, so it never saw the layer that undid them.

    `_project_context` appends the real notice and then trims to fit. It trimmed
    from the FRONT — exactly where `compact`/`truncate` had just pinned the goal
    turn — so a snug window silently dropped the one turn the whole run serves.
    Measured 2026-08-28: the goal sentence was absent from the projection across
    a wide band of windows, in BOTH strategies. The cause was a notice envelope
    reserved at 80 tokens against a real ~132, which put the projection over
    budget and woke the backstop.

    This asserts through the seam, which is the only place the bug was visible.
    """
    goal = "GOAL-SENTINEL: build the widget and report when done"
    msgs = [_turn("system", "sys"), _turn("user", goal)] + _body(40)
    out = await _project_context(
        {"context_window": window, "output_reserve": 0, "context_strategy": strategy},
        _cfg(),
        _FakeProvider(),
        "ai",
        _FakeKernel(),
        "cli",
        msgs,
        len(msgs) - 1,
    )
    assert estimate_tokens(out) <= window, "the projection must still fit"
    kept = any(goal in (m.get("content") or "") for m in out)
    assert kept, (
        f"{strategy}@{window}: the pinned goal turn was dropped by the seam — "
        f"kept {[m.get('role') for m in out]}"
    )


# ── the reply tag must not swallow a bracket from the BODY ───────────
#
# MEASURED 2026-09-04. `_REPLY_TAG`'s verb group was `\S+`, which does not exclude `]`,
# so on a reply whose body contains a bracket the group took `reflect]` and `[^\]]*\]`
# closed on the body's bracket instead of the tag's. The verb then read `reflect]` —
# in no vocabulary — so BOTH trimmers silently did nothing. Reflect bodies are rendered
# with `json.dumps`, so a list-valued field (`verbs=[…]`, always present) puts a bracket
# in every one of them: the reflect dedup documented above as reclaiming 42% of the
# window was inert on exactly the replies it exists for.
#
# It survived because every fixture in all three runtimes used a bracket-free body
# ("\nbody"). The runtimes agreed with each other, and none of them with reality.
def test_the_verb_group_stops_at_the_tags_own_bracket():
    from ai_core.core import _REPLY_TAG

    real = '[reply to notes:aa · notes reflect]\nverbs=["read", "write"]  root=.'
    assert _REPLY_TAG.match(real).group(2) == "reflect"
    stamped = '[reply to notes:aa · notes reflect · 2 of 3 in batch ab12]\nv=["x"]'
    assert _REPLY_TAG.match(stamped).group(2) == "reflect"
    assert (
        _REPLY_TAG.match("[reply to a:1 · notes read]\nplain body").group(2) == "read"
    )
    assert _REPLY_TAG.match("[reply to a:1 notes reflect]") is None


def test_a_stale_reflect_whose_body_has_a_bracket_still_dedupes():
    """The end-to-end shape of the same bug: two reflects of one agent, JSON-ish bodies.
    Before the fix BOTH survived into the model's view, every run, forever."""
    body = 'verbs=["read", "write", "patch"]  root=.'
    out = core.dedupe_stale_reads(
        [
            _turn("user", f"[reply to notes:aa · notes reflect]\n{body}"),
            _turn("assistant", "SEEN notes"),
            _turn("user", f"[reply to notes:bb · notes reflect]\n{body}"),
        ]
    )
    kept = [m for m in out if body in (m.get("content") or "")]
    assert len(kept) == 1, f"the stale whole-state reflect was not dropped: {out}"


async def test_context_clear_starts_a_new_thread_without_deleting(seeded_kernel):
    """`context_clear` is a CLEAR-FOLD: one row whose members are every row since the
    previous clear; the view begins at its `[cleared …]` line; nothing is deleted —
    recall/grep reach the members, and `history {fold}` lists them."""
    from _testkit import memory_meta

    k = seeded_kernel
    aid = "ai_clear"
    k.create("ollama_backend.tools", id=aid, **memory_meta(k, aid, {"cli": "*"}))
    await core._save_history(
        aid,
        k,
        [
            {"role": "user", "content": "the codename is HALCYON"},
            {"role": "assistant", "content": "noted"},
        ],
    )
    assert len(await core._load_history(aid, k)) == 2
    out = await core._context_clear(aid, {"why": "fresh start"}, k)
    assert out["rows"] == 2
    hist = await core._load_history(aid, k)
    assert len(hist) == 1 and hist[0]["kind"] == "clear"
    assert hist[0]["content"].startswith(
        f"[cleared {out['cleared']} · 2 rows] fresh start"
    )
    assert len(hist[0]["refs"]) == 2, "the clear-fold's members are the earlier rows"
    assert "HALCYON" not in json.dumps(hist)
    # behind the boundary, everything is still there
    assert len(await core._load_history(aid, k, behind=True)) == 3
    r = await core._recall(aid, {"query": "codename"}, k)
    assert r["total"] == 1
    g = await core._grep(aid, {"query": "halcyon"}, k)
    assert g["total"] == 1
    # the persisted view is emptied, so assembly renders the store from the clear on
    assert (await core._load_view(aid, k))["ids"] == []
    # chatting on: the new thread grows after the mark
    await core._save_history(aid, k, [{"role": "user", "content": "hello again"}])
    assert [r["content"] for r in await core._load_history(aid, k)][-1] == "hello again"
