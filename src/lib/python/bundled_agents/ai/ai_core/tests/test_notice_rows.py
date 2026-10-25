"""Harness text is a NOTICE, not a message. A turn this layer
writes to its own agent — the no-call reminder, a malformed-call report, a held-message
wake — is stored as `kind: "notice"`, so a view can hide it; the model still
reads it as a user turn (Qwen's template takes no system message past the first).
Only the agent itself can mark one: a peer's `notice` is ignored."""

from _testkit import memory_meta
import asyncio

from ai_core import core
from kernel._kernel import _current_sender


class _Script:
    def __init__(self, turns):
        self.turns = list(turns)
        self.calls = 0
        self.incoming: list[str] = []

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.incoming.append(messages[-1]["content"])
        yield self.turns.pop(0) if self.turns else "…nothing left…"


async def _settle(fp, want, timeout=5.0):
    t0 = asyncio.get_event_loop().time()
    while fp.calls < want:
        assert asyncio.get_event_loop().time() - t0 < timeout, fp.incoming
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.2)


def _agent(kernel, aid, extra_callers=None):
    meta = memory_meta(kernel, aid, {"cli": "*", "kernel": "*"})
    if extra_callers:
        meta["callable_by"] = {**(meta.get("callable_by") or {}), **extra_callers}
    kernel.create("ollama_backend.tools", id=aid, context_window=32768, **meta)


async def _rows(kernel, aid) -> list[dict]:
    out = (await kernel.send([{"target": aid, "payload": {"type": "rows"}}]))[0]
    return out["result"]["rows"]


async def _go(kernel, aid, text="go"):
    await kernel.send([{"target": aid, "payload": {"type": "send", "text": text}}])


DONE = '<call name="done"></call>'
E59 = '<tool_call>\n<function=call name="send">\n<target_id>files</target_id>\n</call>'


async def test_the_no_call_reminder_is_a_notice_and_the_task_is_not(
    seeded_kernel, fake_providers
):
    fp = _Script(["just prose", DONE])
    _agent(seeded_kernel, "nt_a")
    fake_providers["nt_a"] = fp
    try:
        await _go(seeded_kernel, "nt_a")
        await _settle(fp, 2)
        users = [r for r in await _rows(seeded_kernel, "nt_a") if r["role"] == "user"]
        assert users[0]["content"] == "go" and "kind" not in users[0], users[0]
        assert users[1]["content"].startswith("[no call was dispatched]"), users[1]
        assert users[1]["kind"] == "notice", users[1]
    finally:
        fake_providers.pop("nt_a", None)


async def test_a_report_batch_is_a_notice_and_a_batch_with_a_reply_is_not(
    seeded_kernel, fake_providers
):
    good = '<call name="send"><target_id>kernel</target_id><type>reflect</type></call>'
    fp = _Script([E59, good + "\n<send><target_id>kernel</target_id></send>", DONE])
    _agent(seeded_kernel, "nt_b")
    fake_providers["nt_b"] = fp
    try:
        await _go(seeded_kernel, "nt_b")
        await _settle(fp, 3)
        users = [r for r in await _rows(seeded_kernel, "nt_b") if r["role"] == "user"]
        report, mixed = users[1], users[2]
        assert report["content"].startswith("[reply to - · - -"), report
        assert report["kind"] == "notice", report
        # a real call's outcome rides this batch: it is a reply, not a notice
        assert "kernel reflect" in mixed["content"], mixed
        assert "kind" not in mixed, mixed
    finally:
        fake_providers.pop("nt_b", None)


async def test_an_empty_await_leaves_a_note_row_not_a_notice_turn(
    seeded_kernel, fake_providers
):
    fp = _Script(['<call name="await"></call>', DONE])
    _agent(seeded_kernel, "nt_c")
    fake_providers["nt_c"] = fp
    try:
        await _go(seeded_kernel, "nt_c")
        await _settle(fp, 1)
        users = [r for r in await _rows(seeded_kernel, "nt_c") if r["role"] == "user"]
        assert [u.get("kind") for u in users] == [None, "note"], users
        assert fp.calls == 1, "no refusal turn was delivered"
    finally:
        fake_providers.pop("nt_c", None)


async def test_a_peer_cannot_mark_its_message_a_notice(seeded_kernel, fake_providers):
    fp = _Script([DONE])
    _agent(seeded_kernel, "nt_d", {"peer_x": "*"})
    # a REAL peer: the gate is two-sided, so the sender must exist and reach nt_d
    seeded_kernel.create("", id="peer_x", can_call={"nt_d": "*"})
    fake_providers["nt_d"] = fp
    token = _current_sender.set("peer_x")
    try:
        await seeded_kernel.send(
            [
                {
                    "target": "nt_d",
                    "payload": {"type": "send", "text": "hidden?", "notice": True},
                }
            ]
        )
    finally:
        _current_sender.reset(token)
    try:
        await _settle(fp, 1)
        users = [r for r in await _rows(seeded_kernel, "nt_d") if r["role"] == "user"]
        assert users[0]["content"].endswith("hidden?") and "kind" not in users[0], users
    finally:
        fake_providers.pop("nt_d", None)


def test_the_notice_flag_is_read_off_the_turn_in_flight():
    core._current["nt_x"] = {"notice": True}
    try:
        assert core._notice_kind("nt_x") == {"kind": "notice"}
    finally:
        core._current.pop("nt_x", None)
    assert core._notice_kind("nt_x") == {}
