"""Deterministic, no-LLM tests for the context-budget primitives, the overflow
strategies (the projection logic), and the Context Protocol seam + verbs (the unified
notice, the `context` events, `recall`, and the derived `last_reaction`). Fast; no
network. The seam/verb tests use a tiny fake kernel + monkeypatched `_load_history`."""

from __future__ import annotations

import json
import types

import ai_core.core as core
from ai_core.context import (
    DEFAULT_CONTEXT_WINDOW,
    ProjectionCtx,
    budget,
    estimate_one,
    estimate_tokens,
    resolve_context_window,
)
from ai_core.core import _compaction_mark, _project_context, _projection
from ai_core.strategies import DEFAULT_STRATEGY, get_strategy, strategy_names
from ai_core.strategies.base import STUB_SUMMARY, Projection


def _turn(role: str, content: str) -> dict:
    return {"role": role, "content": content}


def _body(n: int, size: int = 200) -> list[dict]:
    return [
        _turn("user" if i % 2 == 0 else "assistant", f"turn{i}-" + "x" * size)
        for i in range(n)
    ]


async def _fake_summary(_msgs) -> str:
    return "SUMMARY"


def _ctx(bud: int, recent_n: int = 4, summarize=_fake_summary) -> ProjectionCtx:
    return ProjectionCtx(
        budget=bud, recent_n=recent_n, summarize=summarize, self_id="ai", kernel=None
    )


class _FakeProvider:
    async def chat(self, messages):
        yield "FAKESUMMARY"


class _FakeKernel:
    """Captures `_to_caller` routing: cli events land in `sent`, browser events in
    `emitted`. (`_project_context` only touches the kernel via `_to_caller`.)"""

    def __init__(self):
        self.sent: list[tuple] = []
        self.emitted: list[tuple] = []

    async def send(self, target, payload):
        self.sent.append((target, payload))
        return {}

    async def emit(self, target, payload):
        self.emitted.append((target, payload))


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


def test_registry_no_memgpt():
    # memgpt collapsed into the universal seam notice — it is no longer a strategy.
    assert set(strategy_names()) == {"compact", "truncate"}
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
    assert proj.body == body[-4:]  # recent verbatim, NO summary turn in the body
    # the strategy never fabricates the old leading marker turn
    assert not any("[Earlier conversation summary]" in m["content"] for m in proj.body)


async def test_truncate_returns_projection_no_summarizer():
    called = []

    async def spy(_msgs):
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


async def test_tool_pairing_never_orphans_role_tool():
    body = [
        _turn("user", "u0"),
        {"role": "assistant", "content": "a", "tool_calls": [{"id": "1"}]},
        {"role": "tool", "tool_call_id": "1", "content": "r"},
        _turn("assistant", "done"),
        _turn("user", "u-live"),
    ]
    # recent_n=3 would start the recent window on the role:tool (idx 2); must snap back.
    proj = await get_strategy("compact")(body, [], {}, _ctx(bud=1_000_000, recent_n=3))
    assert not (proj.body and proj.body[0].get("role") == "tool")


async def test_summarizer_failure_degrades_to_stub():
    async def boom(_msgs):
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
    assert n["role"] == "user" and "tool_calls" not in n
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
    out = await _project_context({}, _cfg(), _FakeProvider(), "ai", k, "cli", msgs)
    assert out == msgs  # unchanged
    assert _projection["ai"] == {"fired": False}
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
    )
    assert estimate_tokens(out) <= 500  # fits the window now
    assert out[0]["role"] == "system"  # system block preserved
    assert (
        out[1]["role"] == "user" and "[context-notice]" in out[1]["content"]
    )  # notice
    proj = _projection["ai"]
    assert proj["fired"] is True and proj["strategy"] == "compact"
    assert proj["summarized"] is True and proj["dropped_turns"] > 0
    # public projection record stays a clean summary — no internal scan bookkeeping
    assert "fired_at_index" not in proj and "client_id" not in proj
    # the private reaction cursor holds the bookkeeping instead
    assert _compaction_mark["ai"]["client_id"] == "cli"
    assert "fired_at_index" in _compaction_mark["ai"]
    # the context:compacted event reached the caller (cli → kernel.send)
    ctx_events = [p for t, p in k.sent if p.get("type") == "context"]
    assert len(ctx_events) == 1
    ev = ctx_events[0]
    assert ev["phase"] == "compacted" and ev["detail"]["strategy"] == "compact"
    assert ev["detail"]["dropped_turns"] > 0


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
    )
    assert isinstance(out, dict) and "bogus" in out["error"]
    assert [p for t, p in k.sent if p.get("type") == "context"] == []


# ── recall verb (read-only page-back over the durable store) ────────


def _send_turn(target, payload):
    """An assistant tool-call turn in RAW text shape (as the store records it now):
    the `<tool_call>` envelope inline in the assistant content."""
    from ai_core.tool_parse import render_tool_call

    return {
        "role": "assistant",
        "content": render_tool_call("send", {"target_id": target, "payload": payload}),
    }


async def test_recall_filters_paginates_and_caps(monkeypatch):
    store = [
        _turn("user", "the project codename is HALCYON"),
        _turn("assistant", "noted"),
        _turn("user", "tell me about bicycles"),
        _turn("assistant", "x" * 5000),  # bulky — must be capped
        _turn("user", "what about codename again"),
    ]

    async def fake_load(self_id, kernel, client_id):
        return store

    monkeypatch.setattr(core, "_load_history", fake_load)

    # substring query (case-insensitive) finds both codename turns
    r = await core._recall("ai", {"query": "codename"}, None)
    assert r["total"] == 2 and r["client_id"] == "cli"
    assert all("codename" in m["content"].lower() for m in r["messages"])
    assert r["messages"][0]["index"] == 0  # store index preserved

    # limit + truncated flag
    r2 = await core._recall("ai", {"limit": 1}, None)
    assert len(r2["messages"]) == 1 and r2["truncated"] is True

    # before paging (only turns with index < 2)
    r3 = await core._recall("ai", {"before": 2}, None)
    assert {m["index"] for m in r3["messages"]} == {0, 1}

    # the bulky turn is content-capped in the reply (never the store)
    rbig = await core._recall("ai", {"query": "xxxx"}, None)
    assert len(rbig["messages"][0]["content"]) <= 2000


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
        _send_turn("mem", {"type": "set", "key": "codename", "value": "HALCYON"}),
        _turn("assistant", "It's HALCYON."),
    ]

    async def fake_load(self_id, kernel, client_id):
        return store

    monkeypatch.setattr(core, "_load_history", fake_load)
    _projection["ai"] = {"fired": True}
    _compaction_mark["ai"] = {"fired_at_index": 1, "client_id": "cli"}
    r = await core._derive_reaction("ai", None)
    assert r == {"recalled": True, "persisted": True, "recall_count": 1}


# ── cli context render ──────────────────────────────────────────────


async def test_cli_context_renders_both_phases(capsys):
    import cli.tools as ct

    await ct._context(
        "cli",
        {
            "type": "context",
            "phase": "compacted",
            "source": "ai",
            "detail": {"strategy": "compact", "dropped_turns": 9},
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
