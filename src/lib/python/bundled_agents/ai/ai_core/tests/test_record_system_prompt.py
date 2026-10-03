"""The record's `system_prompt` field (owner, 2026-10-03, option b).

Set on an ai_core agent's record, it REPLACES the default system block on EVERY turn
of that agent — a main turn, a nudge, and a turn a reply wakes — read LIVE each
turn. Precedence: the per-call `send` argument (that delivery) > the record field >
the default block. A value the record cannot use is refused AT THE WRITE; `null`
clears it. These drive the REAL turn path (`send` → `_run` → `_assemble`) with the
provider faked and every request recorded."""

from __future__ import annotations

import asyncio

import pytest
from _testkit import memory_meta

from ai_core.schema import SYSTEM_PROMPT_REFUSAL
from ai_core.tool_parse import render_call

DONE = '<call name="done"></call>'
LIST = render_call(
    "send",
    {
        "target_id": "kernel_state",
        "payload": {"type": "list_agents"},
        "reply_to": "self",
    },
)


class _Fake:
    """A scripted stream per `chat()`; records the messages of every request."""

    def __init__(self, scripts: list[str]):
        self._scripts = list(scripts)
        self.requests: list[list[dict]] = []

    async def chat(self, messages, max_tokens=None):
        self.requests.append(list(messages))
        yield self._scripts.pop(0) if self._scripts else DONE


def _ai(k, aid: str, **meta) -> None:
    rec = k.create(
        "ollama_backend.tools",
        id=aid,
        endpoint="http://127.0.0.1:9/stub",
        model="stub",
        context_window=32768,
        **meta,
        **memory_meta(k, aid, {"cli": "*"}),
    )
    assert rec.get("id") == aid, rec


async def _send(k, aid: str, **extra) -> None:
    out = (
        await k.send(
            [{"target": aid, "payload": {"type": "send", "text": "go", **extra}}]
        )
    )[0]
    assert out["status"] == "completed", out


async def _op(k, payload) -> dict:
    return (await k.send([{"target": "kernel", "payload": payload}]))[0]["result"]


async def _until(fp: _Fake, n: int, timeout: float = 5.0) -> None:
    end = asyncio.get_event_loop().time() + timeout
    while len(fp.requests) < n:
        assert asyncio.get_event_loop().time() < end, (
            f"chain stalled at {len(fp.requests)} of {n} passes"
        )
        await asyncio.sleep(0.02)


def _sys(req: list[dict]) -> str:
    assert req[0]["role"] == "system"
    return req[0]["content"]


async def test_the_record_prompt_rides_main_reply_woken_and_nudge_turns(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    _ai(k, "sp_ai", system_prompt="REC-ROLE")
    # pass 1: a call; pass 2: woken by its reply, answers in prose (no call);
    # pass 3: the nudge; it ends.
    fp = fake_providers["sp_ai"] = _Fake([LIST, "just prose", DONE])
    await _send(k, "sp_ai")
    await _until(fp, 3)
    main, woken, nudged = fp.requests
    assert "· kernel_state list_agents · ↑" in woken[-1]["content"], woken[-1]
    assert "[no call was dispatched]" in nudged[-1]["content"], nudged[-1]
    assert [_sys(r) for r in fp.requests] == ["REC-ROLE"] * 3


async def test_the_per_call_argument_beats_the_record(seeded_kernel, fake_providers):
    k = seeded_kernel
    _ai(k, "sp_call", system_prompt="REC-ROLE")
    fp = fake_providers["sp_call"] = _Fake([LIST, DONE])
    await _send(k, "sp_call", system_prompt="CALL-ROLE")
    await _until(fp, 2)
    # the argument rides that delivery's chain — its reply-woken turn too
    assert [_sys(r) for r in fp.requests] == ["CALL-ROLE", "CALL-ROLE"]
    # a later delivery without the argument is back on the record's
    await _send(k, "sp_call")
    await _until(fp, 3)
    assert _sys(fp.requests[2]) == "REC-ROLE"


async def test_update_agent_takes_effect_on_the_next_turn(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    _ai(k, "sp_up")
    fp = fake_providers["sp_up"] = _Fake([DONE, DONE, DONE])
    await _send(k, "sp_up")
    default = _sys(fp.requests[0])
    assert default.strip() and default != "NEW-ROLE"
    up = await _op(
        k, {"type": "update_agent", "id": "sp_up", "system_prompt": "NEW-ROLE"}
    )
    assert up.get("updated") is True, up
    await _send(k, "sp_up")
    assert _sys(fp.requests[1]) == "NEW-ROLE"
    # null clears it — the default block again
    up = await _op(k, {"type": "update_agent", "id": "sp_up", "system_prompt": None})
    assert up.get("updated") is True, up
    await _send(k, "sp_up")
    assert _sys(fp.requests[2]) == default


@pytest.mark.parametrize("bad", [5, "", "   ", ["x"], {"a": 1}])
async def test_a_bad_value_is_refused_at_the_write(seeded_kernel, bad):
    k = seeded_kernel
    meta = memory_meta(k, "sp_bad", {"cli": "*"})
    out = await _op(
        k,
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": "ollama_backend.tools",
            "id": "sp_bad",
            "endpoint": "http://127.0.0.1:9/stub",
            "model": "stub",
            "system_prompt": bad,
            **meta,
        },
    )
    assert out.get("error") == SYSTEM_PROMPT_REFUSAL, out
    _ai(k, "sp_ok", system_prompt="FINE")
    out = await _op(k, {"type": "update_agent", "id": "sp_ok", "system_prompt": bad})
    assert out.get("error") == SYSTEM_PROMPT_REFUSAL, out
    assert k.get("sp_ok")["system_prompt"] == "FINE"


def test_the_refusal_text_is_exact():
    assert SYSTEM_PROMPT_REFUSAL == (
        "system_prompt must be a non-empty string (it replaces the default system "
        "block on every turn) — null clears it"
    )


async def test_recall_sizes_the_block_the_turn_uses(seeded_kernel, fake_providers):
    """`context_recall` budgets the body against the system block the turn will
    actually carry — a short record prompt leaves more room than the default block."""
    k = seeded_kernel
    _ai(k, "sp_rc_default")
    _ai(k, "sp_rc_short", system_prompt="R")
    budgets = {}
    for aid in ("sp_rc_default", "sp_rc_short"):
        fp = fake_providers[aid] = _Fake([DONE])
        await _send(k, aid)
        await _until(fp, 1)
        out = (
            await k.send(
                [{"target": aid, "payload": {"type": "context_recall", "dry": True}}]
            )
        )[0]
        assert out["status"] == "completed", out
        budgets[aid] = out["result"]["budget"]
    assert budgets["sp_rc_short"] > budgets["sp_rc_default"], budgets
