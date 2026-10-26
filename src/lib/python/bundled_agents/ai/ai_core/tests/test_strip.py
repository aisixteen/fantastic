"""Deterministic, no-LLM tests for `strip` — the ledger at mixed resolution.

The claims: reply BODIES are stubbed oldest-first in one batch (hysteresis), the model's
own turns are never touched, a stub carries the call's title, the set is monotone so the
rendered prefix is byte-stable between strip events, and the cut is only the second stage.
"""

from __future__ import annotations

import json

from ai_core.context import ProjectionCtx, estimate_tokens
from ai_core.core import _clean_title, _recall
from ai_core.strategies import get_strategy
from ai_core.strategies.strip import LOW, _stripped

from _fake import FakeCtx as _K  # noqa: E402

BIG = "x" * 6000


def _ctx(bud, self_id="ai"):
    return ProjectionCtx(
        budget=bud, recent_n=4, summarize=None, self_id=self_id, kernel=None
    )


def _turn(role, content):
    return {"role": role, "content": content}


def _reply(pid, title, body=BIG, verb="read"):
    t = (
        f'[reply to {pid} · notes {verb} · "{title}"]'
        if title
        else f"[reply to {pid} · notes {verb}]"
    )
    return _turn("user", f"{t}\n{body}")


def _run(n=8, live="write the report"):
    body = [_turn("user", "GOAL: survey then report")]
    for i in range(n):
        body.append(
            _turn(
                "assistant",
                f'Reading filler_{i}. <call name="send"><target_id>notes</target_id></call>',
            )
        )
        body.append(_reply(f"ai:{i:04x}", f"survey filler_{i}"))
        body.append(_turn("assistant", f"filler_{i} was padding."))
    body.append(_turn("user", live))
    return body


def setup_function(_):
    _stripped.clear()


async def test_under_budget_nothing_is_stripped():
    body = _run(2)
    proj = await get_strategy("strip")(body, [], {}, _ctx(100_000))
    assert proj.stripped == 0
    assert [m["content"] for m in proj.body] == [m["content"] for m in body]


async def test_over_budget_strips_oldest_bodies_in_one_batch_down_to_low():
    body = _run(8)
    full = estimate_tokens(body)
    bud = int(full * 0.7)  # over by ~30%
    proj = await get_strategy("strip")(body, [], {}, _ctx(bud))
    assert proj.stripped > 0
    out = estimate_tokens(proj.body)
    # HYSTERESIS: went well below the line, not just under it.
    assert out <= bud * LOW + 200, (out, bud)
    # OLDEST FIRST, CONTIGUOUS: stripped pids are a prefix of the reply order.
    stubbed = [i for i in range(8) if "(stripped:" in proj.body[2 + 3 * i]["content"]]
    assert stubbed == list(range(len(stubbed))), stubbed
    assert proj.omitted_marker is False, "no cut was needed"


async def test_a_stripped_block_keeps_its_tag_and_names_what_it_cost():
    body = _run(8)
    proj = await get_strategy("strip")(
        body, [], {}, _ctx(int(estimate_tokens(body) * 0.5))
    )
    first_stub = proj.body[2]["content"]
    assert first_stub.startswith('[reply to ai:0000 · notes read · "survey filler_0"]')
    assert "(stripped: 6000 bytes" in first_stub
    assert BIG not in first_stub


async def test_only_a_delivered_reply_is_strippable():
    body = _run(8)
    proj = await get_strategy("strip")(
        body, [], {}, _ctx(int(estimate_tokens(body) * 0.5))
    )
    before = [m["content"] for m in body if m["role"] == "assistant"]
    after = [m["content"] for m in proj.body if m["role"] == "assistant"]
    assert before == after


async def test_goal_and_live_turn_are_never_stripped():
    body = _run(8)
    body[0] = _reply("goal:0", "the goal itself")  # a reply-shaped goal turn
    body[-1] = _reply("live:0", "a reply-shaped live turn")
    proj = await get_strategy("strip")(
        body, [], {}, _ctx(int(estimate_tokens(body) * 0.4))
    )
    assert BIG in proj.body[0]["content"]
    assert BIG in proj.body[-1]["content"]


async def test_stripping_is_monotone_and_the_render_is_byte_stable_between_events():
    """The KV-cache property: between strip events the same body renders the same bytes,
    and a later projection never UN-strips (which would be an edit inside the prefix)."""
    body = _run(8)
    bud = int(estimate_tokens(body) * 0.6)
    a = await get_strategy("strip")(body, [], {}, _ctx(bud))
    b = await get_strategy("strip")(
        body, [], {}, _ctx(bud)
    )  # same inputs, no new event
    assert json.dumps(a.body) == json.dumps(b.body)
    # Now the budget RELAXES; nothing may come back.
    c = await get_strategy("strip")(body, [], {}, _ctx(bud * 4))
    assert json.dumps(c.body) == json.dumps(a.body)


async def test_a_second_event_only_extends_the_stripped_run():
    body = _run(8)
    bud1 = int(estimate_tokens(body) * 0.8)
    a = await get_strategy("strip")(body, [], {}, _ctx(bud1))
    n1 = a.stripped
    bud2 = int(estimate_tokens(body) * 0.4)
    b = await get_strategy("strip")(body, [], {}, _ctx(bud2))
    assert b.stripped > n1
    # the first n1 stubs are byte-identical to before — the prefix extended, no holes
    assert [m["content"] for m in b.body[: 2 + 3 * n1]] == [
        m["content"] for m in a.body[: 2 + 3 * n1]
    ]


async def test_when_every_body_is_stripped_and_prose_still_overflows_the_cut_is_stage_two():
    body = [_turn("user", "GOAL")]
    for i in range(40):
        body.append(_turn("assistant", f"long deliberation {i} " + "y" * 800))
        body.append(_reply(f"ai:{i:04x}", f"t{i}", body="z" * 50))
    body.append(_turn("user", "now?"))
    proj = await get_strategy("strip")(body, [], {}, _ctx(3000))
    assert proj.omitted_marker is True, (
        "prose alone overflowed; the cut must have fired"
    )
    assert proj.body[0]["content"] == "GOAL"
    assert proj.body[-1]["content"] == "now?"
    assert estimate_tokens(proj.body) <= 3000


async def test_per_agent_state_does_not_leak_across_agents():
    body = _run(8)
    await get_strategy("strip")(
        body, [], {}, _ctx(int(estimate_tokens(body) * 0.5), "a")
    )
    fresh = await get_strategy("strip")(body, [], {}, _ctx(100_000, "b"))
    assert fresh.stripped == 0


def test_clean_title_matches_python():
    assert _clean_title("read filler_2] then\nrun") == "read filler_2  then run"
    assert _clean_title("   ") is None
    assert _clean_title(42) is None
    assert len(_clean_title("t" * 500)) == 140


# ── recall: a page fits the window it is going into ──────────────────


async def test_a_recall_page_is_trimmed_to_half_the_window_and_says_so(monkeypatch):

    turns = [_turn("user", f"t{i} " + "w" * 8000) for i in range(20)]
    k = _K(window=12000)
    for t in turns:
        k.rows_put("ai", t)
    r = await _recall("ai", {"client_id": "c", "limit": 20}, k)
    msgs = r["messages"]
    assert 0 < len(msgs) < 20
    assert estimate_tokens(msgs) <= 6000
    assert r["truncated"] is True
    assert "before=" in r["note"] and str(msgs[0]["index"]) in r["note"]
    # the NEWEST of what was asked for is what survives
    assert msgs[-1]["index"] == 19


def test_a_tag_that_is_not_a_reply_block_yields_no_pid():
    from ai_core.strategies.strip import _PID, _render

    assert _PID.match("just some prose") is None
    # the ` · ` separator is required
    assert _PID.match("[reply to 9]") is None
    assert _PID.match("[reply to 9 · notes read]") is not None
    # and such a block is never stripped, whatever the stripped set holds
    for text in ("just some prose", "[reply to 9]\nbody"):
        body = [_turn("user", text)]
        out, n, saved = _render(body, {9, "just"})
        assert out == body and (n, saved) == (0, 0), (text, out)


def test_context_notice_shapes():
    from ai_core.core import _context_notice

    n = _context_notice("compact", "MY SUMMARY", False, 7)
    assert n["role"] == "user" and "tool_calls" not in n
    c = n["content"]
    assert "[context-notice]" in c and "MY SUMMARY" in c
    assert "memory agent" in c
    # THE RECALL AFFORDANCE IN THE ONE GRAMMAR, never a JSON shape
    assert "send your OWN id `<type>recall</type>` (optional `<query>`, `<limit>`)" in c
    assert "{type:'recall'" not in c
    assert "7 earlier turn" in c
    assert "STRIPPED" not in c, "no stripped line when nothing was stripped"
    t = _context_notice("truncate", None, True, 3)["content"]
    assert "omitted in place" in t and "Summary of the dropped span" not in t
    # a strip reports what left the view, and how to get it back
    s = _context_notice("strip", None, False, 0, None, 4, 27_000)["content"]
    assert (
        "4 reply bod(ies) were STRIPPED to their tag+title line (27000 bytes left "
        "your view)"
    ) in s, s
    assert (
        "grep finds a line in a body held this run — after a restart, re-read it "
        "from its origin."
    ) in s, s
