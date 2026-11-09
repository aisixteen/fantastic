"""#568: the AI-to-AI send deadline (root `send_timeout`), the dead letter and the
withdrawal at expiry, the deadline the model is told, and `status` counting held
senders. Twin of the Rust `tests_deadline.rs`."""

from __future__ import annotations

import asyncio
import os
import time

import pytest

from _testkit import as_agent, memory_meta, wire

from ai_core import core

DONE = '<call name="done"></call>'


class _Script:
    """A scripted mind: yields the queued replies, records every pass; every pass
    sleeps `delay` first."""

    def __init__(self, turns, delay: float = 0.0):
        self.turns = list(turns)
        self.calls = 0
        self.seen: list[list[dict]] = []
        self.delay = delay

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.seen.append(list(messages))
        if self.delay:
            await asyncio.sleep(self.delay)
        yield self.turns.pop(0) if self.turns else DONE

    def request(self, n) -> str:
        return str(self.seen[n])


async def _settle(fp, want, timeout=5.0):
    t0 = asyncio.get_event_loop().time()
    while fp.calls < want:
        assert asyncio.get_event_loop().time() - t0 < timeout, (
            f"stalled at {fp.calls} passes"
        )
        await asyncio.sleep(0.02)


async def _until(cond, what, timeout=5.0):
    t0 = asyncio.get_event_loop().time()
    while not cond():
        assert asyncio.get_event_loop().time() - t0 < timeout, f"timed out: {what}"
        await asyncio.sleep(0.02)


def _agent(kernel, aid, can_call=None):
    kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(kernel, aid, {"cli": "*", **(can_call or {})}),
    )


def _pair(kernel, a, b):
    """Two AIs, each granted the other on both ends."""
    _agent(kernel, a)
    _agent(kernel, b)
    wire(kernel, a, b)
    wire(kernel, b, a)


def _send(kernel, aid, text, client_id, timeout=None):
    item = {
        "target": aid,
        "payload": {"type": "send", "text": text, "client_id": client_id},
    }
    if timeout is not None:
        item["timeout"] = timeout
    return asyncio.ensure_future(kernel.send([item]))


def _call(target, text, timeout=None):
    t = f"<timeout>{timeout}</timeout>" if timeout is not None else ""
    return (
        f'<call name="send"><routing><target_id>{target}</target_id>{t}'
        f"<reply_to>self</reply_to></routing><payload><type>send</type>"
        f"<text>{text}</text></payload></call>"
    )


async def test_a_message_held_behind_a_busy_target_is_withdrawn_into_the_dead_letter_at_its_deadline(
    seeded_kernel, fake_providers
):
    aid = "dl_held"
    fp = _Script([DONE, DONE], delay=1.5)
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        first = _send(seeded_kernel, aid, "one", "o")
        await _settle(fp, 1)
        t0 = time.monotonic()
        out = (await _send(seeded_kernel, aid, "two", "t", timeout=0.3))[0]
        assert out["status"] == "timed_out", out
        pid = out["pid"]
        assert pid is not None
        assert time.monotonic() - t0 < 1.2, "queue time counts"
        assert core._chain_of(aid)["held"] == []
        rows = await core._load_history(aid, seeded_kernel)
        withdrawn = [r for r in rows if r.get("kind") == "withdrawn"]
        assert len(withdrawn) == 1, rows
        assert f"dead letter {pid}" in withdrawn[0]["content"]
        two = next(r for r in rows if r.get("content") == "two")
        assert withdrawn[0]["parent"] == two["id"]
        await first
        await asyncio.sleep(0.5)
        assert fp.calls == 1, "no pass may answer the withdrawn message"
    finally:
        fake_providers.pop(aid, None)


async def test_a_message_whose_pass_is_running_at_its_deadline_is_cut_into_the_dead_letter(
    seeded_kernel, fake_providers
):
    """REGRESSION PIN: it holds without #568's change."""
    aid = "dl_cut"
    fp = _Script(["late"], delay=1.5)
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        out = (await _send(seeded_kernel, aid, "go", "o", timeout=0.3))[0]
        assert out["status"] == "timed_out", out
        assert out["pid"] is not None
        assert "response" not in (out["result"] or {})
        await _until(lambda: aid not in core._current, "the generation to stop")
        rows = await core._load_history(aid, seeded_kernel)
        assert any(r.get("kind") == "stopped" for r in rows), rows
    finally:
        fake_providers.pop(aid, None)


async def test_status_counts_held_messages_and_names_their_senders(
    seeded_kernel, fake_providers
):
    aid, peer = "dl_status", "dl_status_peer"
    _pair(seeded_kernel, aid, peer)
    fp = _Script([DONE, DONE], delay=1.0)
    fake_providers[aid] = fp
    try:
        first = _send(seeded_kernel, aid, "one", "o")
        await _settle(fp, 1)
        with as_agent(seeded_kernel, peer):
            held = _send(seeded_kernel, aid, "two", "pc")
        await _until(lambda: len(core._chain_of(aid)["held"]) >= 1, "a held message")
        t0 = time.monotonic()
        bare = (
            await seeded_kernel.send([{"target": aid, "payload": {"type": "status"}}])
        )[0]["result"]
        assert time.monotonic() - t0 < 0.2
        assert bare["generating"] is True
        assert bare["others_pending"] == 1
        assert bare["pending_from"] == [peer]
        mine = (
            await seeded_kernel.send(
                [{"target": aid, "payload": {"type": "status", "client_id": "pc"}}]
            )
        )[0]["result"]
        assert "two" in mine["mine_pending"][0]["text"]
        assert mine["others_pending"] == 0
        assert mine["pending_from"] == [peer]
        await first
        await held
    finally:
        fake_providers.pop(aid, None)


async def test_the_route_deadline_takes_the_root_field_then_the_env_then_600(
    seeded_kernel, monkeypatch
):
    monkeypatch.setattr(core, "SEND_TIMEOUT", 45.0)
    assert core._route_send_timeout(seeded_kernel) == 45.0
    try:
        for given, want in [
            (7, 7.0),
            (0.5, 0.5),
            (-1, 45.0),
            ("x", 45.0),
            (True, 45.0),
        ]:
            seeded_kernel._meta["send_timeout"] = given
            assert core._route_send_timeout(seeded_kernel) == want, given
    finally:
        seeded_kernel._meta.pop("send_timeout", None)
    monkeypatch.setattr(core, "SEND_TIMEOUT", 600.0)
    assert core._route_send_timeout(seeded_kernel) == 600.0


async def test_a_send_reply_states_its_deadline_and_a_per_call_timeout_overrides_it(
    seeded_kernel, fake_providers
):
    aid, peer = "dl_told", "dl_told_peer"
    _pair(seeded_kernel, aid, peer)
    seeded_kernel._meta["send_timeout"] = 5
    fp = _Script([_call(peer, "a") + _call(peer, "b", 30), DONE])
    fake_providers[aid] = fp
    fake_providers[peer] = _Script(["ok", "ok"])
    try:
        await _send(seeded_kernel, aid, "go", "o")
        await _settle(fp, 2)
        req = fp.request(1)
        assert "(deadline 5.0s)" in req, req
        assert "(deadline 30s)" in req, req
        assert "resend it, or drop it with" in fp.request(0)
    finally:
        seeded_kernel._meta.pop("send_timeout", None)
        fake_providers.pop(aid, None)
        fake_providers.pop(peer, None)


async def test_a_send_past_its_deadline_tells_the_sender_its_dead_letter_and_can_be_resent(
    seeded_kernel, fake_providers
):
    aid, peer = "dl_resend", "dl_resend_peer"
    _pair(seeded_kernel, aid, peer)
    wire(seeded_kernel, aid, seeded_kernel.id, ["ack_deadletter"])
    seeded_kernel._meta["send_timeout"] = 0.3
    fp = _Script([_call(peer, "ping"), DONE])
    fpp = _Script([DONE, DONE, "resent answer"], delay=1.5)
    fake_providers[aid], fake_providers[peer] = fp, fpp
    try:
        busy = _send(seeded_kernel, peer, "busy", "o")
        await _settle(fpp, 1)
        await _send(seeded_kernel, aid, "go", "o")
        await _settle(fp, 2)
        req = fp.request(1)
        marker = f"your message to {peer} hit its 0.3s deadline → dead letter "
        assert marker in req, req
        tail = req.split(marker, 1)[1]
        pid = tail[: len(tail) - len(tail.lstrip("0123456789"))]
        assert pid, req
        rows = await core._load_history(peer, seeded_kernel)
        assert any(
            r.get("kind") == "withdrawn" and pid in r["content"] for r in rows
        ), rows
        with as_agent(seeded_kernel, aid):
            acked = (
                await seeded_kernel.send(
                    [
                        {
                            "target": seeded_kernel.id,
                            "payload": {"type": "ack_deadletter", "id": int(pid)},
                        }
                    ]
                )
            )[0]["result"]
        assert acked.get("acked") is True, acked
        await busy
        await _until(lambda: not core._chain_of(peer)["alive"], "the peer idle")
        with as_agent(seeded_kernel, aid):
            resent = (await _send(seeded_kernel, peer, "ping", "r", timeout=10))[0][
                "result"
            ]
        assert "response" in resent, resent
    finally:
        seeded_kernel._meta.pop("send_timeout", None)
        fake_providers.pop(aid, None)
        fake_providers.pop(peer, None)


@pytest.mark.skipif(
    "FANTASTIC_AI_SEND_TIMEOUT" in os.environ, reason="the ceiling is overridden"
)
def test_the_default_send_ceiling_is_a_python_float():
    """Pin (#538): the default per-send ceiling is 600 s, a float. Twin of the Rust
    `the_default_send_ceiling_is_a_python_float`; it fails if the value is lowered."""
    from ai_core import constants

    assert constants.SEND_TIMEOUT == 600.0
    assert repr(constants.SEND_TIMEOUT) == "600.0"
