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
        "ollama_backend.tools",
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


async def test_a_failed_pass_stores_the_incoming_and_an_error_row(
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
        await _say(seeded_kernel, aid, "two")
        assert len(fp.seen) == 2, len(fp.seen)
        prompt = str(fp.seen[1])
        assert "[turn failed]" in prompt and "two" in prompt, fp.seen[1]
    finally:
        fake_providers.pop(aid, None)


async def test_the_failed_turn_status_is_done_error_naming_the_stall(
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
        assert done and done[-1]["detail"]["reason"] == "error", done
        assert "stream_stalled" in done[-1]["detail"]["error"], done
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


async def test_stop_during_a_stalled_stream_ends_the_turn(
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
        assert done and done[-1]["detail"]["reason"] == "error", done
        assert endpoint in done[-1]["detail"]["error"], done
        assert len(_errors(await _rows(seeded_kernel, aid))) == 1
    finally:
        fake_providers.pop(aid, None)
