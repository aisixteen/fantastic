"""TRANSPORT RETRIES (#198 part 2): a turn whose LINK dropped is retried, visibly.

A pass that fails with a transport `ProviderError` (connect failed, the stream cut,
HTTP 502/503/504) is re-run up to the record's `transport_retries` times (0 = off,
max 5), with a 2 s, 4 s, 8 s backoff. Each retry stores a notice row and emits a
`retrying` status. A model error, a non-transport failure and a cancel are never
retried. Port of the Rust #198 tests (fantastic-ai-core `tests_retries.rs`)."""

from __future__ import annotations

from _testkit import memory_meta

from ai_core import core
from ai_core.provider import ProviderError
from ai_core.schema import AI_FIELDS, MISFIRE_LIMIT_REFUSAL, schema, validate

DONE = '<call name="done"></call>'
TRANSPORT_REFUSAL = "transport_retries must be an int from 0 to 5"


def _link(message: str = "connect failed") -> ProviderError:
    return ProviderError(message, transport=True)


def _answer(text: str = "ANSWER") -> list:
    return [text, DONE]


class _Script:
    """Each request takes the next pass: a list whose items are text chunks, or an
    exception raised in place of a chunk. Every request's messages are recorded."""

    def __init__(self, passes):
        self.passes = list(passes)
        self.seen: list[list[dict]] = []

    @property
    def calls(self) -> int:
        return len(self.seen)

    async def chat(self, messages, max_tokens=None):
        self.seen.append(list(messages))
        items = self.passes.pop(0) if self.passes else [DONE]
        for item in items:
            if isinstance(item, Exception):
                raise item
            yield item


def _agent(kernel, aid, retries=None):
    meta = {"context_window": 32768, **memory_meta(kernel, aid, {"cli": "*"})}
    if retries is not None:
        meta["transport_retries"] = retries
    kernel.create("ollama_backend.tools", id=aid, **meta)


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


async def _statuses(kernel, aid) -> list[dict]:
    """The `status` events a watcher on the agent's inbox saw, in order."""
    q = kernel.ctx.inboxes["watch_rt"]
    out = []
    while not q.empty():
        ev = q.get_nowait()
        if ev.get("type") == "status":
            out.append(ev)
    return out


async def _notices(kernel, aid) -> list[dict]:
    rows = await core._refresh_head(aid, kernel)
    return [r for r in rows if r.get("kind") == "notice"]


def _watch(kernel, aid):
    kernel.ctx.ensure_inbox("watch_rt")
    kernel.watch(aid, "watch_rt")


async def test_a_dropped_link_is_retried_and_the_turn_answers(
    seeded_kernel, fake_providers, monkeypatch
):
    monkeypatch.setattr(core, "RETRY_BACKOFF_BASE", 0.005)
    aid = "rt_answers"
    fp = _Script([[_link()], _answer()])
    _agent(seeded_kernel, aid, retries=2)
    _watch(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        reply = await _say(seeded_kernel, aid, "hello")
        assert fp.calls >= 2, f"passes served: {fp.calls}"
        assert "[retry 1/2]" in str(fp.seen[1]), fp.seen[1]
        assert "error" not in reply, reply
    finally:
        fake_providers.pop(aid, None)


async def test_retries_run_out_and_the_turn_fails(
    seeded_kernel, fake_providers, monkeypatch
):
    monkeypatch.setattr(core, "RETRY_BACKOFF_BASE", 0.005)
    aid = "rt_runout"
    fp = _Script([[_link("connect failed")], [_link("connect failed again")]])
    _agent(seeded_kernel, aid, retries=1)
    _watch(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        reply = await _say(seeded_kernel, aid, "hello")
        assert fp.calls == 2, f"passes served: {fp.calls}"
        assert "[retry 1/1]" in str(fp.seen[1]), fp.seen[1]
        assert "[retry 2/" not in str(fp.seen[1]), fp.seen[1]
        assert reply.get("error"), reply
        done = [
            s for s in await _statuses(seeded_kernel, aid) if s.get("phase") == "done"
        ]
        assert done and done[-1]["detail"]["reason"] == "error", done
    finally:
        fake_providers.pop(aid, None)


async def test_a_model_error_is_never_retried(
    seeded_kernel, fake_providers, monkeypatch
):
    monkeypatch.setattr(core, "RETRY_BACKOFF_BASE", 0.005)
    aid = "rt_model"
    fp = _Script([[ProviderError("boom")]])
    _agent(seeded_kernel, aid, retries=3)
    fake_providers[aid] = fp
    try:
        await _say(seeded_kernel, aid, "hello")
        assert fp.calls == 1, f"passes served: {fp.calls}"
        assert "[retry" not in str(fp.seen[0]), fp.seen[0]
    finally:
        fake_providers.pop(aid, None)


async def test_retries_are_off_by_default(seeded_kernel, fake_providers, monkeypatch):
    monkeypatch.setattr(core, "RETRY_BACKOFF_BASE", 0.005)
    aid = "rt_default"
    fp = _Script([[_link()], _answer()])
    _agent(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _say(seeded_kernel, aid, "hello")
        assert fp.calls == 1, f"passes served: {fp.calls}"
    finally:
        fake_providers.pop(aid, None)


async def test_each_retry_is_a_visible_row(seeded_kernel, fake_providers, monkeypatch):
    monkeypatch.setattr(core, "RETRY_BACKOFF_BASE", 0.005)
    aid = "rt_row"
    fp = _Script([[_link()], _answer()])
    _agent(seeded_kernel, aid, retries=2)
    fake_providers[aid] = fp
    try:
        await _say(seeded_kernel, aid, "hello")
        notices = [
            n
            for n in await _notices(seeded_kernel, aid)
            if str(n.get("content", "")).startswith("[retry 1/")
        ]
        assert len(notices) == 1, notices
        assert "connect failed" in notices[0]["content"], notices[0]
    finally:
        fake_providers.pop(aid, None)


async def test_a_retrying_status_names_the_attempt(
    seeded_kernel, fake_providers, monkeypatch
):
    monkeypatch.setattr(core, "RETRY_BACKOFF_BASE", 0.005)
    aid = "rt_status"
    fp = _Script([[_link()], _answer()])
    _agent(seeded_kernel, aid, retries=2)
    _watch(seeded_kernel, aid)
    fake_providers[aid] = fp
    try:
        await _say(seeded_kernel, aid, "hello")
        retrying = [
            s
            for s in await _statuses(seeded_kernel, aid)
            if s.get("phase") == "retrying"
        ]
        assert len(retrying) == 1, retrying
        detail = retrying[0]["detail"]
        assert (detail["attempt"], detail["of"]) == (1, 2), detail
        assert detail["error"] == "connect failed", detail
    finally:
        fake_providers.pop(aid, None)


def test_transport_retries_is_in_the_schema_before_misfire_limit():
    names = [f["name"] for f in AI_FIELDS]
    assert names.index("transport_retries") == names.index("misfire_limit") - 1
    field = next(f for f in AI_FIELDS if f["name"] == "transport_retries")
    assert field["type"] == "int" and field.get("default") == 0, field


def test_transport_retries_validates():
    sch = schema([])
    rec = {"memory_store_id": "m"}

    def refuse(v):
        return validate(sch, "ai_core", {"transport_retries": v}, rec)

    for bad in (6, -1, "x", True):
        assert refuse(bad) == TRANSPORT_REFUSAL, bad
    for good in (0, 5, None):
        assert refuse(good) != TRANSPORT_REFUSAL, good
    assert MISFIRE_LIMIT_REFUSAL not in str(refuse(0))
