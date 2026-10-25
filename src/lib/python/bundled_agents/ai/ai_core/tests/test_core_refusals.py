"""ai_core's hand-built refusals and reply plumbing, pinned on a REAL kernel with a
real ram_store — the provider is the only stand-in.

core.py builds ~20 `{error, reason, …}` dicts by hand (some with codes outside
`kernel._refusal.CODES`, e.g. `memory_store`), routes over-cap replies through
`_origin_line`, and decides row-event bodies with its own copy of the store's
grant semantics. Switching those to `refusal()` / a shared helper (reduction
candidate K) must keep every text, `reason`, extra key and key order below.
"""

from __future__ import annotations

import asyncio
import types

import pytest
from _testkit import memory_meta

from ai_core import core
from ai_core.tool_parse import render_call
from kernel import register_unlisted
from _testkit import as_agent

NAME = "ollama_backend"


class _Script:
    """A provider that plays one scripted turn per `chat` and records what it saw."""

    def __init__(self, *turns, hold: float = 0.0):
        self.turns = list(turns)
        self.seen: list[list[dict]] = []
        self.hold = hold

    async def chat(self, messages, max_tokens=None):
        self.seen.append(list(messages))
        if self.hold:
            await asyncio.sleep(self.hold)
        for chunk in self.turns.pop(0) if self.turns else ['<call name="done"></call>']:
            yield chunk


def _ai(k, aid: str, provider=None, **meta) -> str:
    m = memory_meta(k, aid, can_call=meta.pop("can_call", None))
    rec = k.create(
        "ollama_backend.tools",
        id=aid,
        endpoint="http://127.0.0.1:9/stub",
        model="stub",
        context_window=32768,
        **m,
        **meta,
    )
    assert rec.get("id") == aid, rec
    if provider is not None:
        _fakes[aid] = provider
    return aid


#: The test's `fake_providers` table, reachable from the helpers here.
_fakes: dict = {}


@pytest.fixture(autouse=True)
def _seat_fakes(fake_providers, monkeypatch):
    monkeypatch.setitem(globals(), "_fakes", fake_providers)


async def _call(k, target, payload, as_=None) -> dict:
    if as_ is None:
        return (await k.send([{"target": target, "payload": payload}]))[0]["result"]
    with as_agent(k, as_):
        return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


# ─── send ────────────────────────────────────────────────────────────────────


async def test_send_refuses_a_cycle_and_an_over_deep_chain(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_s", _Script())
    r = await _call(
        k, "kp_s", {"type": "send", "text": "x", "_call_stack": ["a", "kp_s"]}
    )
    assert r == {
        "error": f"{NAME}: cycle detected",
        "response": "",
        "cycle": ["a", "kp_s", "kp_s"],
        "client_id": "cli",
    }
    deep = [f"a{i}" for i in range(core.MAX_CALL_DEPTH)]
    r = await _call(
        k,
        "kp_s",
        {"type": "send", "text": "x", "_call_stack": deep, "client_id": "tab"},
    )
    assert r == {
        "error": f"{NAME}: max call depth {core.MAX_CALL_DEPTH} reached",
        "response": "",
        "client_id": "tab",
    }


async def test_send_without_a_store_fails_before_the_turn(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    _ai(k, "kp_ns", _Script())
    k.ctx.update("kp_ns", memory_store_id="")
    r = await _call(k, "kp_ns", {"type": "send", "text": "x"})
    assert r == {
        "error": (
            "kp_ns: no `memory_store_id` — an AI agent keeps its conversation in a "
            "memory store and runs without none. Create one (e.g. a ram_store), set "
            "memory_store_id on kp_ns, and grant kp_ns can_call on it AND the store "
            "callable_by for it"
        ),
        "reason": "missing_arg",
        "client_id": "cli",
    }
    assert fake_providers["kp_ns"].seen == []


async def test_a_store_that_refuses_is_a_memory_store_failure(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_ms", _Script())
    # close the store's door for this agent
    cb = dict(k.ctx.agents["memory"]._meta["callable_by"])
    cb.pop("kp_ms")
    k.ctx.update("memory", callable_by=cb)
    r = await _call(k, "kp_ms", {"type": "send", "text": "x"})
    assert r["reason"] == "memory_store" and r["client_id"] == "cli", r
    assert list(r) == ["error", "reason", "client_id"]
    assert r["error"].startswith(
        "kp_ms: memory store 'memory' read failed: memory does not accept 'read' "
        "from kp_ms (hint: "
    ), r


async def test_a_turn_over_the_send_timeout(seeded_kernel, monkeypatch):
    k = seeded_kernel
    monkeypatch.setattr(core, "SEND_TIMEOUT", 0.05)
    _ai(k, "kp_to", _Script(["late"], hold=5))
    r = await _call(k, "kp_to", {"type": "send", "text": "x"})
    assert r == {"error": "send: timeout after 0.05s", "client_id": "cli"}


async def test_an_unknown_verb_names_the_backend(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_u")
    assert await _call(k, "kp_u", {"type": "zzz"}) == {
        "error": f"{NAME}: unknown type 'zzz'",
        "reason": "unhandled",
        "verb": "zzz",
    }


# ─── inquiry ─────────────────────────────────────────────────────────────────


async def test_inquiry_texts(seeded_kernel, fake_providers):
    """`inquiry` on an AI is ACCEPTED at once (fully async) and
    runs as its own turn, the question under a head naming who asked and the pid;
    the answer — given in any later turn — is covered in `test_inquiry_async.py`.
    An operator's inquiry has no conversation to land in: the answer is an event."""
    k = seeded_kernel
    _ai(k, "kp_iq0", _Script())
    for bad in ("", "   ", None, 5):
        assert await _call(k, "kp_iq0", {"type": "inquiry", "text": bad}) == {
            "error": "inquiry: `text` must be a non-empty string",
            "reason": "missing_arg",
            "arg": "text",
        }
    q = {
        "type": "inquiry",
        "text": "ship?",
        "request_id": "r1",
        "options": ["a", "b"],
        "action": {"type": "deploy"},
    }
    _ai(k, "kp_iq3", _Script([render_call("inquiry_answer", {"answer": "ship it"})]))
    got = await _call(k, "kp_iq3", q)
    assert got["accepted"] is True and got["request_id"] == "r1", got
    pid = got["re"]
    for _ in range(100):
        if fake_providers["kp_iq3"].seen:
            break
        await asyncio.sleep(0.02)
    first_turn = fake_providers["kp_iq3"].seen[0]
    last_user = [m for m in first_turn if m["role"] == "user"][-1]["content"]
    assert last_user.startswith(f"[inquiry from the operator · re {pid} — "), last_user
    assert last_user.endswith(
        'ship?\n\nOptions: a, b\n\nProposed action: {"type": "deploy"}'
    ), last_user


# ─── history / grep / recall-side verbs ──────────────────────────────────────


async def test_grep_and_history_bad_args(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_g")
    for q in (None, "", "  "):
        assert await _call(k, "kp_g", {"type": "grep", "query": q}) == {
            "error": "grep: `query` is required",
            "reason": "bad_arg",
            "arg": "query",
        }
    for ids in ("1", [1, "x"], [True], {"a": 1}):
        assert await _call(k, "kp_g", {"type": "grep", "query": "a", "ids": ids}) == {
            "error": "grep: `ids` must be a list of message ids / pids (ints)",
            "reason": "bad_arg",
            "arg": "ids",
        }, ids
    assert await _call(k, "kp_g", {"type": "grep", "query": "nothing"}) == {
        "matches": [],
        "total": 0,
        "truncated": False,
    }


async def test_watch_refusals(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_w", callable_by={"viewer": ["watch", "unwatch"]})
    assert await _call(k, "kp_w", {"type": "watch"}) == {
        "error": "watch: the operator must name a `watcher` inbox",
        "reason": "missing_arg",
        "arg": "watcher",
    }
    assert await _call(k, "kp_w", {"type": "watch", "watcher": "ghost_box"}) == {
        "error": "watch: 'ghost_box' has no registered inbox, so nothing would read the row events",
        "reason": "no_consumer",
        "hint": "register the reader first — ctx.ensure_inbox('ghost_box') — then watch",
    }
    k.create(None, id="viewer", can_call={"kp_w": ["watch", "unwatch"]})
    assert await _call(
        k, "kp_w", {"type": "watch", "watcher": "other"}, as_="viewer"
    ) == {
        "error": "watch: an agent watches for ITSELF — `watcher` is the operator's",
        "reason": "unauthorized",
    }
    k.ctx.ensure_inbox("viewer")
    assert await _call(k, "kp_w", {"type": "watch", "body": 1}, as_="viewer") == {
        "watching": True,
        "watcher": "viewer",
        "body": True,
    }
    assert await _call(k, "kp_w", {"type": "unwatch"}, as_="viewer") == {
        "watching": False,
        "watcher": "viewer",
    }


# ─── an over-cap reply: stored in RAM, its ORIGIN line delivered ─────────────


_big_mod = types.ModuleType("kp_big_probe.tools")


async def _big_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "big replies", "verbs": {}}
    lines = "\n".join(f"line {i} needle-{i}" for i in range(3000))
    if payload.get("with_origin"):
        return {"text": lines, "origin": "  big   file.txt\n lines 1-3000  v7 "}
    return {"text": lines}


_big_mod.handler = _big_handler
_big_mod.VERBS = {}
register_unlisted("kp_big_probe.tools", _big_mod)


async def _turn_reply(k, aid: str, payload: dict) -> str:
    """Run one AI turn that calls `big` with `payload`; return the reply text the
    model is handed on its NEXT turn."""
    prov = _Script([render_call("send", {"target_id": "big", "payload": payload})])
    _fakes[aid] = prov
    await _call(k, aid, {"type": "send", "text": "fetch it"})
    for _ in range(200):
        if len(prov.seen) >= 2:
            break
        await asyncio.sleep(0.01)
    assert len(prov.seen) >= 2, "the reply was never delivered"
    return [m for m in prov.seen[1] if m["role"] == "user"][-1]["content"]


async def test_an_over_cap_reply_is_held_and_named_by_the_call(seeded_kernel):
    k = seeded_kernel
    k.create("kp_big_probe.tools", id="big", callable_by={"kp_oc": "*"})
    _ai(k, "kp_oc", can_call={"big": "*"})
    text = await _turn_reply(k, "kp_oc", {"type": "fetch", "n": 1})
    pid = int(text.split("<ids><item>")[1].split("</item>")[0])
    size = len(core.render_reply(await _big_handler("big", {"type": "fetch"}, k)))
    assert (
        f"(settled, {size} bytes — NOT inlined, held in memory only until this agent "
        'restarts. origin: big fetch {"n": 1} — re-send it; the output may differ. '
        "To find a line in it: send yourself <type>grep</type> <query>substring"
        f"</query> <ids><item>{pid}</item></ids>)"
    ) in text, text
    # the held body is greppable by its pid
    r = await _call(k, "kp_oc", {"type": "grep", "query": "needle-2999", "ids": [pid]})
    assert r["total"] == 1 and r["matches"][0]["id"] == pid, r
    assert r["matches"][0]["text"].endswith("line 2999 needle-2999"), r


async def test_an_over_cap_reply_with_an_origin_uses_it(seeded_kernel):
    k = seeded_kernel
    k.create("kp_big_probe.tools", id="big", callable_by={"kp_oo": "*"})
    _ai(k, "kp_oo", can_call={"big": "*"})
    text = await _turn_reply(k, "kp_oo", {"type": "fetch", "with_origin": True})
    assert "origin: big file.txt lines 1-3000 v7. To find a line" in text, text


# ─── route_reply's own error units (a call it could not dispatch) ────────────


async def _turn_text(k, aid: str, call_text: str) -> str:
    prov = _Script([call_text])
    _fakes[aid] = prov
    await _call(k, aid, {"type": "send", "text": "go"})
    for _ in range(200):
        if len(prov.seen) >= 2:
            break
        await asyncio.sleep(0.01)
    assert len(prov.seen) >= 2, "the error unit was never delivered"
    return [m for m in prov.seen[1] if m["role"] == "user"][-1]["content"]


async def test_a_send_with_no_payload_names_the_target(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_np")
    text = await _turn_text(
        k, "kp_np", '<call name="send"><target_id>somebody</target_id></call>'
    )
    want = core.render_reply(
        {
            "error": (
                "send to 'somebody' carried no payload, so nothing was dispatched. "
                "A call names its verb in `<type>`, e.g. <type>reflect</type>."
            ),
            "target": "somebody",
            "you_sent": {"target_id": "somebody"},
        }
    )
    assert want in text, text


async def test_the_model_facing_error_strings_are_verbatim(seeded_kernel, monkeypatch):
    """The model-facing strings are prompts: pinned as literal text so tidying one
    shows up as a failing test."""
    from ai_core.tool_parse import _HINT, malformed_calls

    k = seeded_kernel
    _ai(k, "kp_vb")
    # no target
    call = {"id": 1, "name": "send", "arguments": {"payload": {"type": "reflect"}}}
    with monkeypatch.context() as m:
        m.setattr(core, "terminated_calls", lambda a: [(call, "")])
        text = await _turn_text(k, "kp_vb", "go")
    assert (
        "no target_id, so nothing was dispatched — a send must name the AGENT it "
        'goes to. Shape: <call name="send"><target_id>agent</target_id>'
        "<type>verb</type></call>. Send `kernel` <type>reflect</type> for the "
        "list of agent ids."
    ) in text, text
    # no payload
    text = await _turn_text(
        k, "kp_vb", '<call name="send"><target_id>totals</target_id></call>'
    )
    assert (
        "send to 'totals' carried no payload, so nothing was dispatched. A call "
        "names its verb in `<type>`, e.g. <type>reflect</type>."
    ) in text, text
    # the no-call reminder
    assert core._NO_CALL_NUDGE == (
        "[no call was dispatched] Your answer emitted no <call> element, so "
        "NOTHING ran — a call written in your thinking does not run: write it in "
        "your answer, after the thinking. If the task is unfinished, emit that call "
        "now. If it is finished, end the chain explicitly: "
        '<call name="done"></call>'
    )
    # the parser's malformed-call sentence reaches the model verbatim as the body
    bad_text = '<call name="send"><type>reflect</type></call>'
    bad = malformed_calls(bad_text)
    assert len(bad) == 1, bad
    assert bad[0]["error"].startswith("call NOT dispatched: a send names its target")
    assert bad[0]["error"].endswith(f". Nothing ran. {_HINT}")
    text = await _turn_text(k, "kp_vb", bad_text)
    assert bad[0]["error"] in text, text


async def test_target_fallthrough_matches_python_truthiness(seeded_kernel, monkeypatch):
    """`target` is accepted beside `target_id`; a falsy `target_id` yields to it, a
    truthy non-string one does not (it earns the shape error)."""
    k = seeded_kernel
    _ai(k, "kp_tf")
    no_target = "no target_id, so nothing was dispatched"

    async def go(**args) -> str:
        call = {
            "id": 1,
            "name": "send",
            "arguments": {**args, "payload": {"type": "reflect"}},
        }
        monkeypatch.setattr(core, "terminated_calls", lambda a: [(call, "")])
        return await _turn_text(k, "kp_tf", "go")

    ok = await go(target_id="kernel")
    assert no_target not in ok and "[reply to " in ok, ok
    for args in (
        {"target": " kernel "},
        {"target_id": "", "target": "kernel"},
        {"target_id": None, "target": "kernel"},
    ):
        text = await go(**args)
        assert no_target not in text and "[reply to " in text, (args, text)
    assert no_target in await go(target_id=7, target="kernel")
    assert no_target in await go(reply_to="self")


async def test_watch_announces_each_appended_row_until_unwatch(seeded_kernel):
    """`watch` announces every row the agent appends to the watcher's inbox,
    content-free without `body`; `unwatch` stops it."""
    k = seeded_kernel
    _ai(k, "kp_wa", callable_by={"viewer": ["watch", "unwatch"]})
    k.create(None, id="viewer", can_call={"kp_wa": ["watch", "unwatch"]})
    q = k.ctx.ensure_inbox("viewer")
    assert await _call(k, "kp_wa", {"type": "watch"}, as_="viewer") == {
        "watching": True,
        "watcher": "viewer",
        "body": False,
    }
    await core._save_history("kp_wa", k, [{"role": "user", "content": "hello"}])
    stored = await core._load_history("kp_wa", k)
    assert q.qsize() == 1, "one row event"
    assert q.get_nowait() == {
        "type": "row",
        "source": "kp_wa",
        "row": {"id": stored[-1]["id"], "kind": "user", "role": "user", "bytes": 5},
    }, "a content-free digest"
    r = await _call(k, "kp_wa", {"type": "unwatch"}, as_="viewer")
    assert r["watching"] is False, r
    await core._save_history("kp_wa", k, [{"role": "user", "content": "again"}])
    assert q.empty(), "no event after unwatch"
