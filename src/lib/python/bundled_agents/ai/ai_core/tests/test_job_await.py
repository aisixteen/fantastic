"""AWAIT WAITS FOR A RUNNING EXEC JOB (#306). Port of the Rust #306 tests
(fantastic-ai-core `tests_st306.rs`).

An `exec start` that answers `running` used to end its routing turn with nothing
pending, so a bare `await` was refused and nothing woke the owner when the job
ended. Now the owner records each job its own batch started and watches it: `await`
counts the recorded jobs as pending, and the job's end wakes the owner ONCE with
`job_done {job_id, exit_code, tail}`. A deleted owner forgets its jobs."""

from __future__ import annotations

import asyncio
import types

from _testkit import memory_meta
from kernel import register_unlisted

from ai_core import core
from ai_core.tool_parse import render_call

DONE = '<call name="done"></call>'
# The wake's text. The Turns prompt also says `job_done` (no brace), so match the brace.
JOB_DONE = "job_done {"
AWAIT = '<call name="await"></call>'
START = render_call("send", {"target_id": "jw_exec", "payload": {"type": "start"}})

# The fake exec agent's job: `start` answers running; `status` answers running until
# `released` is set, then done with exit 0; `tail` returns one line.
_state = {"released": False}


async def _exec_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "start":
        return {"job_id": 1, "status": "running"}
    if verb == "status":
        await asyncio.sleep(0.02)
        if _state["released"]:
            return {"jobs": [{"status": "done", "exit_code": 0}]}
        return {"jobs": [{"status": "running"}]}
    if verb == "tail":
        return {"lines": [{"line": "hello from the job"}]}
    return {"error": f"unknown verb {verb!r}"}


_exec = types.ModuleType("jw_exec.tools")
_exec.handler = _exec_handler
_exec.VERBS = {}
register_unlisted("jw_exec.tools", _exec)


class _Script:
    """A scripted mind: yields the queued replies, records every request."""

    def __init__(self, turns):
        self.turns = list(turns)
        self.seen: list[list[dict]] = []

    async def chat(self, messages, max_tokens=None):
        self.seen.append(list(messages))
        yield self.turns.pop(0) if self.turns else DONE


def _ai(kernel, aid, script, fake_providers, *, can_call=None):
    fake_providers[aid] = script
    kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(kernel, aid, {"cli": "*", **(can_call or {})}),
    )


def _requests_with(script, needle: str) -> int:
    return sum(
        1 for msgs in script.seen if any(needle in str(m.get("content")) for m in msgs)
    )


async def _until(cond, timeout=5.0):
    t0 = asyncio.get_event_loop().time()
    while not cond():
        assert asyncio.get_event_loop().time() - t0 < timeout, "timed out"
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.1)


async def _say(kernel, aid, text="go"):
    return (
        await kernel.send([{"target": aid, "payload": {"type": "send", "text": text}}])
    )[0]


async def _setup(kernel, fake_providers, owner_turns):
    """The exec agent, and an owner AI that may start jobs on it."""
    _state["released"] = False
    kernel.create(
        "jw_exec.tools",
        id="jw_exec",
        callable_by={"jw_owner": "*", "jw_bystander": "*"},
    )
    owner = _Script(owner_turns)
    _ai(
        kernel,
        "jw_owner",
        owner,
        fake_providers,
        can_call={"jw_exec": "*"},
    )
    return owner


async def _cleanup(kernel, fake_providers):
    _state["released"] = True
    await asyncio.sleep(0.3)
    fake_providers.pop("jw_owner", None)
    fake_providers.pop("jw_idle", None)
    fake_providers.pop("jw_bystander", None)


async def test_await_with_a_running_job_waits_for_it(
    seeded_kernel, fake_providers, monkeypatch
):
    k = seeded_kernel
    statuses: list[tuple[str, str, dict]] = []
    orig = core._emit_status

    async def record(kernel, self_id, client_id, phase, **detail):
        statuses.append((self_id, phase, detail))
        await orig(kernel, self_id, client_id, phase, **detail)

    monkeypatch.setattr(core, "_emit_status", record)
    owner = await _setup(k, fake_providers, [START + "\n" + AWAIT, DONE])
    try:
        await _say(k, "jw_owner")
        await _until(
            lambda: any(
                s == "jw_owner" and d.get("reason") == "awaiting"
                for s, _, d in statuses
            )
        )
        awaiting = [
            d
            for s, _, d in statuses
            if s == "jw_owner" and d.get("reason") == "awaiting"
        ]
        assert awaiting and awaiting[-1].get("pending", 0) >= 1, statuses
        # no refusal, and no job_done wake while the job still runs (the start's own
        # reply may arrive as a turn; that is not a wake)
        assert _requests_with(owner, core._AWAIT_REFUSED) == 0, owner.seen
        assert _requests_with(owner, JOB_DONE) == 0
        assert core._running_jobs.get("jw_owner"), core._running_jobs
    finally:
        await _cleanup(k, fake_providers)


async def test_a_finished_job_wakes_its_owner_once(seeded_kernel, fake_providers):
    k = seeded_kernel
    owner = await _setup(k, fake_providers, [START + "\n" + AWAIT, DONE])
    try:
        await _say(k, "jw_owner")
        await _until(lambda: bool(core._running_jobs.get("jw_owner")))
        _state["released"] = True
        await _until(lambda: _requests_with(owner, JOB_DONE) >= 1)
        await asyncio.sleep(0.3)
        assert _requests_with(owner, JOB_DONE) == 1, owner.seen
        wake = next(
            m["content"]
            for msgs in owner.seen
            for m in msgs
            if JOB_DONE in str(m.get("content"))
        )
        assert '"exit_code": 0' in wake, wake
        assert "hello from the job" in wake, wake
        assert "jw_owner" not in core._running_jobs, core._running_jobs
    finally:
        await _cleanup(k, fake_providers)


async def test_await_with_no_job_and_nothing_pending_still_ends_visibly(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    idle = _Script([AWAIT, DONE])
    _ai(k, "jw_idle", idle, fake_providers)
    try:
        await _say(k, "jw_idle")
        await _until(lambda: _requests_with(idle, core._AWAIT_REFUSED) >= 1)
        assert "jw_idle" not in core._running_jobs, core._running_jobs
    finally:
        await _cleanup(k, fake_providers)


async def test_a_job_started_by_another_agent_does_not_wake_this_one(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    await _setup(k, fake_providers, [START, DONE])
    bystander = _Script([AWAIT, DONE])
    _ai(k, "jw_bystander", bystander, fake_providers)
    try:
        await _say(k, "jw_owner")
        await _until(lambda: bool(core._running_jobs.get("jw_owner")))
        await _say(k, "jw_bystander")
        await _until(lambda: _requests_with(bystander, core._AWAIT_REFUSED) >= 1)
        assert _requests_with(bystander, JOB_DONE) == 0, bystander.seen
        assert "jw_bystander" not in core._running_jobs, core._running_jobs
        assert core._running_jobs.get("jw_owner"), core._running_jobs
    finally:
        await _cleanup(k, fake_providers)


async def test_a_deleted_owner_stops_counting_its_job(seeded_kernel, fake_providers):
    k = seeded_kernel
    owner = await _setup(k, fake_providers, [START + "\n" + AWAIT, DONE])
    try:
        await _say(k, "jw_owner")
        await _until(lambda: bool(core._running_jobs.get("jw_owner")))
        out = (
            await k.send(
                [
                    {
                        "target": "kernel",
                        "payload": {"type": "delete_agent", "id": "jw_owner"},
                    }
                ]
            )
        )[0]["result"]
        assert out.get("deleted"), out
        assert "jw_owner" not in core._running_jobs, core._running_jobs
        _state["released"] = True
        await asyncio.sleep(0.3)
        assert "jw_owner" not in core._running_jobs, core._running_jobs
        assert _requests_with(owner, JOB_DONE) == 0, owner.seen
    finally:
        await _cleanup(k, fake_providers)
