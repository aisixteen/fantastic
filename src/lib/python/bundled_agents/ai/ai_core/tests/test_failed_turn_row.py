"""A turn whose pass FAILS stores its own rows (#177): the incoming message and an
error row naming why, so the next turn's prompt reads the failure and the history
shows it. Nothing the failed turn meant to send was dispatched. A refused record (a
config error — no pass opened) stores nothing. Port of the Rust `store_failed_turn`
tests; also pins the parser, `stop` and `await` behaviours the same issue named."""

from __future__ import annotations

import asyncio

from _testkit import memory_meta

from ai_core import core
from ai_core.provider import ProviderError

DONE = '<call name="done"></call>'
AWAIT = '<call name="await"></call>'
STALL = "x: stream_stalled — http://e sent no bytes for 0.2s; this turn is NOT complete"


class _Script:
    """Each request takes the next pass: text chunks, or an exception raised in place
    of a chunk. Every request's messages are recorded."""

    def __init__(self, passes):
        self.passes = list(passes)
        self.seen: list[list[dict]] = []

    async def chat(self, messages, max_tokens=None):
        self.seen.append(list(messages))
        for item in self.passes.pop(0) if self.passes else [DONE]:
            if isinstance(item, Exception):
                raise item
            yield item


class _Hang:
    """A provider that sends a little, then blocks forever."""

    def __init__(self):
        self.calls = 0

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        yield "so far"
        await asyncio.sleep(3600)
        yield "never"


def _agent(kernel, aid):
    kernel.create(
        "ai_chat.tools",
        id=aid,
        context_window=32768,
        **memory_meta(kernel, aid, {"cli": "*"}),
    )


async def _say(kernel, aid, text):
    return (
        await kernel.send(
            [
                {
                    "target": aid,
                    "payload": {"type": "send", "text": text, "client_id": "tab1"},
                }
            ]
        )
    )[0].get("result")


async def _rows(kernel, aid) -> list[dict]:
    return await core._load_history(aid, kernel)


def _errors(rows) -> list[dict]:
    return [r for r in rows if r.get("kind") == "error"]


async def test_a_failed_pass_stores_an_error_row_and_the_queue_moves_on(
    seeded_kernel, fake_providers
):
    aid = "ft_rows"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = _Script([[ProviderError(STALL, transport=True)]])
    try:
        await _say(seeded_kernel, aid, "one")
        rows = await _rows(seeded_kernel, aid)
        incoming = [r for r in rows if r.get("content") == "one"]
        assert len(incoming) == 1 and incoming[0]["role"] == "user", rows
        errs = _errors(rows)
        assert len(errs) == 1, rows
        assert errs[0]["role"] == "user"
        assert "stream_stalled" in errs[0]["content"]
        assert "nothing this turn meant to send was dispatched" in errs[0]["content"]
        assert errs[0]["content"].startswith("[turn failed] ")
        assert errs[0]["parent"] == incoming[0]["id"]
    finally:
        fake_providers.pop(aid, None)


async def test_the_queue_moves_on_and_the_next_turn_reads_the_failure(
    seeded_kernel, fake_providers
):
    aid = "ft_next"
    _agent(seeded_kernel, aid)
    fp = _Script([[ProviderError(STALL, transport=True)], [DONE]])
    fake_providers[aid] = fp
    try:
        await _say(seeded_kernel, aid, "one")
        for _ in range(250):
            if len(fp.seen) >= 2:
                break
            await asyncio.sleep(0.02)
        assert len(fp.seen) >= 2, len(fp.seen)
        assert "[turn failed]" in str(fp.seen[1]), fp.seen[1]
    finally:
        fake_providers.pop(aid, None)


async def test_a_stalled_api_stream_ends_the_turn_with_an_error_row(
    seeded_kernel, fake_providers
):
    aid = "ft_status"
    _agent(seeded_kernel, aid)
    seeded_kernel.ctx.ensure_inbox("watch_ft")
    seeded_kernel.watch(aid, "watch_ft")
    fake_providers[aid] = _Script([[ProviderError(STALL, transport=True)]])
    try:
        reply = await _say(seeded_kernel, aid, "one")
        assert "stream_stalled" in reply["error"], reply
        q = seeded_kernel.ctx.inboxes["watch_ft"]
        done = []
        while not q.empty():
            ev = q.get_nowait()
            if ev.get("type") == "status" and ev.get("phase") == "done":
                done.append(ev)
        errs = [e for e in done if e["detail"].get("reason") == "error"]
        assert errs, done
        assert "stream_stalled" in errs[0]["detail"]["error"], done
    finally:
        fake_providers.pop(aid, None)


async def test_a_send_timeout_stores_an_error_row(
    seeded_kernel, fake_providers, monkeypatch
):
    monkeypatch.setattr(core, "SEND_TIMEOUT", 0.05)
    aid = "ft_timeout"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = _Hang()
    try:
        reply = await _say(seeded_kernel, aid, "one")
        assert "send: timeout after" in reply["error"], reply
        errs = _errors(await _rows(seeded_kernel, aid))
        assert len(errs) == 1, errs
        assert "send: timeout after" in errs[0]["content"]
    finally:
        fake_providers.pop(aid, None)


async def test_a_refused_record_stores_no_error_row(
    seeded_kernel, fake_providers, monkeypatch
):
    from ai_core import prompt_file

    for n, exc in enumerate(
        (core.MemoryStoreError("store gone"), prompt_file.PromptFileError("no file"))
    ):
        aid = f"ft_refused{n}"
        _agent(seeded_kernel, aid)
        fake_providers[aid] = _Script([[DONE]])

        async def boom(*a, _exc=exc, **k):
            raise _exc

        monkeypatch.setattr(core, "_run", boom)
        try:
            reply = await _say(seeded_kernel, aid, "one")
            assert reply.get("error"), reply
            assert _errors(await _rows(seeded_kernel, aid)) == []
        finally:
            fake_providers.pop(aid, None)


async def test_a_failed_save_is_named_in_the_error(
    seeded_kernel, fake_providers, monkeypatch
):
    aid = "ft_nosave"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = _Script([[ProviderError(STALL, transport=True)]])

    async def refuse(*a, **k):
        raise RuntimeError("store refused")

    monkeypatch.setattr(core, "_save_history", refuse)
    try:
        reply = await _say(seeded_kernel, aid, "one")
        assert "stream_stalled" in reply["error"], reply
        assert (
            "the turn's error row could not be stored: store refused"
            in (reply["error"])
        ), reply
    finally:
        fake_providers.pop(aid, None)


# ─── pins: what #177 named that already holds in Python ─────────────────────


async def test_stop_during_a_stalled_stream_clears_generating(
    seeded_kernel, fake_providers
):
    aid = "ft_stop"
    _agent(seeded_kernel, aid)
    fp = _Hang()
    fake_providers[aid] = fp
    try:
        turn = asyncio.ensure_future(_say(seeded_kernel, aid, "one"))
        for _ in range(200):
            if fp.calls:
                break
            await asyncio.sleep(0.02)
        await asyncio.sleep(0.1)
        stop = (
            await seeded_kernel.send([{"target": aid, "payload": {"type": "stop"}}])
        )[0]["result"]
        assert stop["stopped"] is True, stop
        await asyncio.wait_for(turn, 5)
        status = (
            await seeded_kernel.send([{"target": aid, "payload": {"type": "status"}}])
        )[0]["result"]
        assert status["generating"] is False, status
        assert [
            r for r in await _rows(seeded_kernel, aid) if r.get("kind") == "stopped"
        ]
    finally:
        fake_providers.pop(aid, None)


async def test_an_empty_await_ends_the_chain_with_a_stored_note(
    seeded_kernel, fake_providers
):
    aid = "ft_await"
    _agent(seeded_kernel, aid)
    fp = _Script([[AWAIT], [DONE]])
    fake_providers[aid] = fp
    try:
        await _say(seeded_kernel, aid, "one")
        await asyncio.sleep(0.4)
        assert len(fp.seen) == 1, "the empty await ended the chain: no second pass"
        notes = [r for r in await _rows(seeded_kernel, aid) if r.get("kind") == "note"]
        assert [n["content"] for n in notes] == [core._EMPTY_AWAIT_NOTE]
    finally:
        fake_providers.pop(aid, None)


async def test_a_queued_send_waits_behind_a_stalled_turn_and_runs_when_the_window_ends(
    seeded_kernel, fake_providers
):
    aid = "ft_queued"
    endpoint = "https://api.example/v1/messages"

    class _StallThenDone:
        def __init__(self):
            self.calls = 0
            self.seen: list[list[dict]] = []

        async def chat(self, messages, max_tokens=None):
            self.calls += 1
            self.seen.append(list(messages))
            if self.calls == 1:
                await asyncio.sleep(0.5)
                raise ProviderError(
                    f"x_backend: stream_stalled — {endpoint} sent no bytes for 0.5s; "
                    "this turn is NOT complete",
                    transport=True,
                )
            yield DONE

    _agent(seeded_kernel, aid)
    fp = fake_providers[aid] = _StallThenDone()
    seeded_kernel.ctx.ensure_inbox("watch_q")
    seeded_kernel.watch(aid, "watch_q")

    def send(text, cid):
        return asyncio.ensure_future(
            seeded_kernel.send(
                [
                    {
                        "target": aid,
                        "payload": {"type": "send", "text": text, "client_id": cid},
                    }
                ]
            )
        )

    try:
        first = send("one", "probe_a")
        for _ in range(200):
            if fp.calls:
                break
            await asyncio.sleep(0.02)
        second = send("two", "probe_b")
        await asyncio.sleep(0.2)
        q = seeded_kernel.ctx.inboxes["watch_q"]
        events = []
        while not q.empty():
            events.append(q.get_nowait())
        assert any(
            e.get("type") == "status"
            and e.get("phase") == "queued"
            and e.get("client_id") == "probe_b"
            for e in events
        ), events
        assert fp.calls == 1, "the second send ran while the first turn was open"
        r1 = (await asyncio.wait_for(first, 10))[0]["result"]
        r2 = (await asyncio.wait_for(second, 10))[0]["result"]
        assert "stream_stalled" in r1["error"], r1
        assert "error" not in r2 and "response" in r2, r2
        assert fp.calls == 2
        assert "[turn failed]" in str(fp.seen[1]), fp.seen[1]
        while not q.empty():
            events.append(q.get_nowait())
        done = [
            e
            for e in events
            if e.get("type") == "status"
            and e.get("phase") == "done"
            and e.get("client_id") == "probe_a"
        ]
        # the retried turn (#533) closes with its own `ok`: pick the failed one
        errs = [e for e in done if e["detail"].get("reason") == "error"]
        assert errs, done
        assert endpoint in errs[0]["detail"]["error"], done
        assert len(_errors(await _rows(seeded_kernel, aid))) == 1
    finally:
        fake_providers.pop(aid, None)


# ─── a failed turn settles its sender and tells its watchers (#496) ─────────


def _done_events(kernel, inbox) -> list[dict]:
    q = kernel.ctx.inboxes[inbox]
    out = []
    while not q.empty():
        ev = q.get_nowait()
        if ev.get("type") == "status" and ev.get("phase") == "done":
            out.append(ev)
    return out


async def test_a_timed_out_turn_settles_its_sender_with_reason_timeout_and_is_not_retried(
    seeded_kernel, fake_providers, monkeypatch
):
    monkeypatch.setattr(core, "SEND_TIMEOUT", 0.1)
    aid = "ft_to_reason"
    _agent(seeded_kernel, aid)
    seeded_kernel.ctx.ensure_inbox("watch_to")
    seeded_kernel.watch(aid, "watch_to")
    fp = fake_providers[aid] = _Hang()
    try:
        reply = await _say(seeded_kernel, aid, "one")
        assert reply["reason"] == "timeout", reply
        assert reply["client_id"] == "tab1", reply
        assert reply["error"].startswith("send: timeout after 0.1s"), reply
        done = _done_events(seeded_kernel, "watch_to")
        assert done and done[-1]["detail"]["reason"] == "timeout", done
        assert len(_errors(await _rows(seeded_kernel, aid))) == 1
        await asyncio.sleep(0.4)
        assert fp.calls == 1, "the failed turn was run again"
    finally:
        fake_providers.pop(aid, None)


async def test_a_held_sender_settles_with_reason_timeout_when_the_turn_that_reads_it_times_out(
    seeded_kernel, fake_providers, monkeypatch
):
    monkeypatch.setattr(core, "SEND_TIMEOUT", 0.5)
    aid = "ft_held_to"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = _Hang()

    def send(text, cid):
        return asyncio.ensure_future(
            seeded_kernel.send(
                [
                    {
                        "target": aid,
                        "payload": {"type": "send", "text": text, "client_id": cid},
                    }
                ]
            )
        )

    try:
        first = send("one", "o")
        await asyncio.sleep(0.2)
        second = send("two", "t")
        one = (await asyncio.wait_for(first, 10))[0]["result"]
        assert one["reason"] == "timeout", one
        two = (await asyncio.wait_for(second, 10))[0]["result"]
        assert two["reason"] == "timeout", two
        assert two["client_id"] == "t", two
    finally:
        fake_providers.pop(aid, None)


async def test_a_held_sender_settles_with_transport_error_when_the_turn_that_reads_it_fails(
    seeded_kernel, fake_providers
):
    aid = "ft_held_fail"
    _agent(seeded_kernel, aid)

    class _SlowFail:
        calls = 0

        async def chat(self, messages, max_tokens=None):
            self.calls += 1
            if self.calls == 1:
                await asyncio.sleep(0.6)
            raise ProviderError("boom", transport=False)
            yield  # pragma: no cover

    fake_providers[aid] = _SlowFail()

    def send(text, cid):
        return asyncio.ensure_future(
            seeded_kernel.send(
                [
                    {
                        "target": aid,
                        "payload": {"type": "send", "text": text, "client_id": cid},
                    }
                ]
            )
        )

    try:
        first = send("one", "o")
        await asyncio.sleep(0.2)
        second = send("two", "t")
        one = (await asyncio.wait_for(first, 10))[0]
        assert one["status"] == "failed", one
        two = (await asyncio.wait_for(second, 10))[0]["result"]
        assert two["reason"] == "transport_error", two
        assert two["client_id"] == "t", two
        assert "boom" in two["error"], two
    finally:
        fake_providers.pop(aid, None)


async def test_a_parent_awaiting_a_child_that_times_out_is_handed_the_timeout(
    seeded_kernel, fake_providers, monkeypatch
):
    from _testkit import wire

    from ai_core.tool_parse import render_call

    monkeypatch.setattr(core, "SEND_TIMEOUT", 0.5)
    parent, child = "ft_parent", "ft_child"
    _agent(seeded_kernel, parent)
    _agent(seeded_kernel, child)
    wire(seeded_kernel, parent, child)
    wire(seeded_kernel, child, parent)
    call = render_call(
        "send",
        {"target_id": child, "timeout": 5, "payload": {"type": "send", "text": "work"}},
    )
    pf = fake_providers[parent] = _Script([[call], [DONE]])

    class _SlowChild:
        calls = 0

        async def chat(self, messages, max_tokens=None):
            self.calls += 1
            await asyncio.sleep(2.5)
            yield "never"

    cf = fake_providers[child] = _SlowChild()
    try:
        await _say(seeded_kernel, parent, "go")
        for _ in range(300):
            if len(pf.seen) >= 2:
                break
            await asyncio.sleep(0.05)
        assert len(pf.seen) >= 2, "the parent was never handed the child's failure"
        read = str(pf.seen[1])
        assert "[reply to " in read and "send: timeout after 0.5s" in read, pf.seen[1]
        assert cf.calls == 1, "the child's turn was retried"
    finally:
        fake_providers.pop(parent, None)
        fake_providers.pop(child, None)


# ─── a failed turn is retried on its own (#533) ─────────────────────────────


async def _poll(cond, seconds=5.0):
    for _ in range(int(seconds / 0.02)):
        if cond():
            return True
        await asyncio.sleep(0.02)
    return False


async def test_a_failed_pass_is_retried_on_its_own_and_its_answer_is_stored(
    seeded_kernel, fake_providers
):
    aid = "ft_retry_once"
    _agent(seeded_kernel, aid)
    fake_providers[aid] = _Script(
        [[ProviderError("mock_backend: boom")], ["RETRIED-ANSWER " + DONE]]
    )
    try:
        out = await _say(seeded_kernel, aid, "go")
        assert out["raised"] is True, out
        rows: list[dict] = []
        for _ in range(250):
            rows = await _rows(seeded_kernel, aid)
            if any("RETRIED-ANSWER" in str(r.get("content")) for r in rows):
                break
            await asyncio.sleep(0.02)
        incoming = next(i for i, r in enumerate(rows) if r.get("content") == "go")
        error = next(i for i, r in enumerate(rows) if r.get("kind") == "error")
        answer = next(
            i for i, r in enumerate(rows) if "RETRIED-ANSWER" in str(r.get("content"))
        )
        assert incoming < error < answer, rows
    finally:
        fake_providers.pop(aid, None)


async def test_a_pass_cut_at_max_tokens_gets_the_next_turn_with_the_failure_text(
    seeded_kernel, fake_providers
):
    aid = "ft_retry_cut"
    _agent(seeded_kernel, aid)
    fp = _Script(
        [
            [
                RuntimeError(
                    "mock_backend: the turn was cut at max_tokens=16384 and is NOT complete"
                )
            ],
            [DONE],
        ]
    )
    fake_providers[aid] = fp
    try:
        await _say(seeded_kernel, aid, "go")
        assert await _poll(lambda: len(fp.seen) >= 2), len(fp.seen)
        seen = str(fp.seen[1])
        assert "[turn failed]" in seen and "cut at max_tokens=16384" in seen, seen
    finally:
        fake_providers.pop(aid, None)


async def test_failures_in_a_row_end_in_gave_up_not_a_loop(
    seeded_kernel, fake_providers
):
    aid = "ft_retry_bound"
    _agent(seeded_kernel, aid)
    seeded_kernel.ctx.ensure_inbox("watch_rb")
    seeded_kernel.watch(aid, "watch_rb")
    fp = _Script(
        [[ProviderError("mock_backend: boom")]] * core.MAX_SILENT_TURNS + [[DONE]]
    )
    fake_providers[aid] = fp
    seen: list[dict] = []

    def gave_up():
        q = seeded_kernel.ctx.inboxes["watch_rb"]
        while not q.empty():
            ev = q.get_nowait()
            if ev.get("type") == "status" and ev.get("phase") == "done":
                seen.append(ev)
        return [e for e in seen if e["detail"].get("reason") == "gave_up"]

    try:
        await _say(seeded_kernel, aid, "go")
        assert await _poll(lambda: bool(gave_up()), 15), seen
        assert gave_up()[0]["detail"]["error"] == (
            "The LLM was not able to answer — 10 turns in a row failed or produced "
            "no call. Last failure: mock_backend: boom"
        )
        await asyncio.sleep(0.3)
        assert len(fp.seen) == core.MAX_SILENT_TURNS, len(fp.seen)
    finally:
        fake_providers.pop(aid, None)


async def test_a_cancelled_turn_is_not_retried(seeded_kernel, fake_providers):
    aid = "ft_retry_cancel"
    _agent(seeded_kernel, aid)
    fp = _Hang()
    fake_providers[aid] = fp
    try:
        turn = asyncio.ensure_future(_say(seeded_kernel, aid, "go"))
        assert await _poll(lambda: fp.calls >= 1)
        await seeded_kernel.send([{"target": aid, "payload": {"type": "stop"}}])
        await asyncio.wait_for(turn, 5)
        await asyncio.sleep(0.9)
        assert fp.calls == 1, "a cancelled turn was retried"
        assert _errors(await _rows(seeded_kernel, aid)) == []
    finally:
        fake_providers.pop(aid, None)


async def test_a_held_message_is_answered_by_the_retried_turn(
    seeded_kernel, fake_providers
):
    aid = "ft_retry_held"
    _agent(seeded_kernel, aid)

    class _FailThenAnswer:
        def __init__(self):
            self.calls = 0

        async def chat(self, messages, max_tokens=None):
            self.calls += 1
            await asyncio.sleep(0.4)
            if self.calls == 1:
                raise ProviderError("mock_backend: boom")
            yield "HELD-ANSWER " + DONE

    fp = fake_providers[aid] = _FailThenAnswer()

    def send(text, cid):
        return asyncio.ensure_future(
            seeded_kernel.send(
                [
                    {
                        "target": aid,
                        "payload": {"type": "send", "text": text, "client_id": cid},
                    }
                ]
            )
        )

    try:
        first = send("one", "probe_a")
        assert await _poll(lambda: fp.calls >= 1)
        second = send("two", "probe_b")
        assert await _poll(lambda: len(core._chain_of(aid)["held"]) >= 1)
        first_out = (await asyncio.wait_for(first, 10))[0]["result"]
        assert first_out["raised"] is True, first_out
        second_out = (await asyncio.wait_for(second, 10))[0]["result"]
        assert "error" not in second_out, second_out
        assert "HELD-ANSWER" in second_out["response"], second_out
        assert fp.calls == 2
    finally:
        fake_providers.pop(aid, None)
