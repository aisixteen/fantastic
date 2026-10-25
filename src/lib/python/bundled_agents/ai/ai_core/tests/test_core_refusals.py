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
from ai_core.tool_parse import render_call, render_reply
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


async def test_check_reports_the_chain_and_never_mutates(seeded_kernel):
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
# twin: unknown_type_is_pythons_repr_and_shutdown_is_unknown
# twin: unknown_type_wire_shape_is_byte_exact
# twin: unknown_type_wire_shape_is_byte_exact


async def test_an_unknown_verb_names_the_backend(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_u")
    assert await _call(k, "kp_u", {"type": "zzz"}) == {
        "error": f"{NAME}: unknown type 'zzz'",
        "reason": "unhandled",
        "verb": "zzz",
    }


# ─── inquiry ─────────────────────────────────────────────────────────────────


async def test_inquiry_turn_text_uses_python_str_and_json_dumps(seeded_kernel, fake_providers):
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


# ─── twins of the Rust send / routing tests (fantastic-ai-core/src/tests.rs) ──


class _Boom:
    """A provider whose stream dies with an error no layer maps."""

    async def chat(self, messages, max_tokens=None):
        raise RuntimeError("provider exploded")
        yield  # pragma: no cover — makes this an async generator


def _watch(k, aid: str) -> asyncio.Queue:
    """The agent's events, as a watcher's inbox sees them."""
    box = f"watch_{aid}"
    q = k.ctx.ensure_inbox(box)
    k.watch(aid, box)
    return q


def _drain(q) -> list[dict]:
    out = []
    while not q.empty():
        out.append(q.get_nowait())
    return out


def _done_statuses(events) -> list[dict]:
    return [e for e in events if e.get("type") == "status" and e.get("phase") == "done"]


async def test_unmapped_provider_error_settles_raised(seeded_kernel):
    """A provider failure no layer maps SETTLES `{error: 'RuntimeError: …', raised}`
    (the raise carries the type) with no client_id; the one `status(done)` event
    carries `str(e)` alone."""
    k = seeded_kernel
    _ai(k, "kp_raise", _Boom())
    q = _watch(k, "kp_raise")
    r = await _call(k, "kp_raise", {"type": "send", "text": "go", "client_id": "probe"})
    assert r["error"] == "RuntimeError: provider exploded", r
    assert r["raised"] is True, r
    assert "client_id" not in r, r
    done = _done_statuses(_drain(q))
    assert len(done) == 1, done
    assert done[0]["detail"]["reason"] == "error", done
    assert done[0]["detail"]["error"] == "provider exploded", done


async def test_a_projection_refusal_is_returned_verbatim_and_does_not_raise(
    seeded_kernel,
):
    """A context-projection refusal is not a provider failure: the seam's dict comes
    out of the send VERBATIM — no `status(done)` closes a turn that never opened a
    provider pass, no `client_id` is stapled on, nothing raised."""
    k = seeded_kernel
    # a window the system block cannot fit: the `too_small` failsafe, before the model
    _ai(k, "kp_toosmall", _Script(["never reached"]))
    k.ctx.update("kp_toosmall", context_window=300)
    q = _watch(k, "kp_toosmall")
    r = await _call(
        k, "kp_toosmall", {"type": "send", "text": "go", "client_id": "probe"}
    )
    assert "context_insufficient" in r["error"], r
    assert "raised" not in r, f"a refusal is not a raise: {r}"
    assert "client_id" not in r, f"the seam dict comes back bare: {r}"
    events = _drain(q)
    assert _done_statuses(events) == [], "no turn opened, so nothing closes one"
    assert any(
        e.get("type") == "context" and e.get("phase") == "too_small" for e in events
    ), events
    assert _fakes["kp_toosmall"].seen == [], "the model was never called"


async def _ps(k) -> list[dict]:
    return (await k.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0][
        "result"
    ]["tasks"]


async def _until(cond, what: str, timeout: float = 5.0) -> None:
    end = asyncio.get_event_loop().time() + timeout
    while not cond():
        assert asyncio.get_event_loop().time() < end, what
        await asyncio.sleep(0.01)


async def test_routing_is_its_own_ps_row(seeded_kernel):
    """Routing is a `route_reply` task of its own: while the slow peer works, `ps`
    names it, and no row for the finished generation is still labelled `send`."""
    k = seeded_kernel
    peer = _Script(['slow peer <call name="done"></call>'], hold=0.8)
    _ai(k, "kp_rt_peer", peer, callable_by={"kp_rt": "*"})
    ping = render_call(
        "send",
        {
            "target_id": "kp_rt_peer",
            "payload": {"type": "send", "text": "ping"},
            "reply_to": "null",
        },
    )
    _ai(k, "kp_rt", _Script([ping]), can_call={"kp_rt_peer": "*"})
    await _call(k, "kp_rt", {"type": "send", "text": "go"})
    await _until(lambda: peer.seen, "the peer was never reached")
    tasks = await _ps(k)
    assert any(t["agent"] == "kp_rt" and t["verb"] == "route_reply" for t in tasks), (
        f"routing must be its own `route_reply` row: {tasks}"
    )
    assert not any(t["agent"] == "kp_rt" and t["verb"] == "send" for t in tasks), (
        f"the generation is finished — nothing may still be labelled `send`: {tasks}"
    )


async def test_delivery_is_a_deliver_reply_task_not_awaited(seeded_kernel):
    """Each delivery is its own `deliver_reply` kernel task, on the TARGET, and the
    router does not wait for the target's (slow) turn."""
    k = seeded_kernel
    peer = _Script(['peer ack <call name="done"></call>'], hold=0.8)
    _ai(k, "kp_dl_peer", peer, callable_by={"kp_dl": "*"})
    call = render_call(
        "send",
        {
            "target_id": "kernel_state",
            "payload": {"type": "list_agents"},
            "reply_to": "kp_dl_peer",
        },
    )
    _ai(
        k,
        "kp_dl",
        _Script([call]),
        can_call={"kernel_state": "*", "kp_dl_peer": "*"},
    )
    started = asyncio.get_event_loop().time()
    await _call(k, "kp_dl", {"type": "send", "text": "go", "client_id": "probe"})
    # the routing pass itself is detached, so give it a moment to spawn
    await asyncio.sleep(0.15)
    assert asyncio.get_event_loop().time() - started < 0.7, "the router awaited it"
    tasks = await _ps(k)
    rows = [t for t in tasks if t["verb"] == "deliver_reply"]
    assert rows, f"no deliver_reply task row: {tasks}"
    assert rows[0]["agent"] == "kp_dl_peer", f"the task belongs to the TARGET: {rows}"


async def test_a_batch_that_includes_a_peer_ai_still_delivers(seeded_kernel):
    """Seven calls in one turn, one of them another AI (its dispatch starts a
    generation of its own): the whole batch still settles and the follow-up turn
    runs."""
    k = seeded_kernel
    brain = _Script(['brain says hi <call name="done"></call>'])
    _ai(k, "kp_pa_brain", brain, callable_by={"kp_pa": "*"})
    reads = [
        render_call(
            "send", {"target_id": "kernel_state", "payload": {"type": "reflect"}}
        )
        for _ in range(6)
    ]
    ask = render_call(
        "send",
        {
            "target_id": "kp_pa_brain",
            "payload": {"type": "send", "text": "what shape?", "client_id": "probe"},
        },
    )
    sa = _Script(["\n".join(reads + [ask])], ['seen <call name="done"></call>'])
    _ai(k, "kp_pa", sa, can_call={"kernel_state": "*", "kp_pa_brain": "*"})
    await _call(k, "kp_pa", {"type": "send", "text": "explore", "client_id": "probe"})
    await _until(
        lambda: len(sa.seen) >= 2,
        "turn 2 never ran — a batch containing a peer AI delivered nothing",
    )
    assert brain.seen, "the peer AI was dispatched"


async def test_a_malformed_timeout_arg_settles_the_kernels_refusal(seeded_kernel):
    """Routing forwards `timeout` RAW: a non-numeric value reaches the kernel's own
    refusal instead of being dropped and defaulting the call as if never written."""
    k = seeded_kernel
    _ai(k, "kp_bt", can_call={"kernel_state": "*"})
    call = render_call(
        "send",
        {
            "target_id": "kernel_state",
            "payload": {"type": "list_agents"},
            "reply_to": "self",
            "timeout": "soon",
        },
    )
    turn = await _turn_text(k, "kp_bt", call)
    assert "timeout must be a positive number of seconds" in turn, turn
    assert "'soon'" in turn, f"must echo the raw value: {turn}"


async def test_two_identical_turns_get_different_batch_tokens(seeded_kernel):
    """A FRESH token per turn: two turns that made the SAME calls must be
    distinguishable at a glance."""
    k = seeded_kernel
    one = render_call(
        "send",
        {
            "target_id": "kernel_state",
            "payload": {"type": "list_agents"},
            "reply_to": "self",
        },
    )
    twice = one + "\n" + one
    prov = _Script([twice], [twice], ['enough <call name="done"></call>'])
    _ai(k, "kp_tok", prov, can_call={"kernel_state": "*"})
    await _call(k, "kp_tok", {"type": "send", "text": "go", "client_id": "probe"})
    await _until(lambda: len(prov.seen) >= 3, "the second batch never landed")

    def token(turn_index: int) -> str:
        last = [m for m in prov.seen[turn_index] if m["role"] == "user"][-1]["content"]
        mark = "1 of 2 in batch "
        assert mark in last, f"no batch stamp in: {last}"
        return last.split(mark)[1].split("]")[0]

    first, second = token(1), token(2)
    assert first != second, "two identical turns must be distinguishable"


async def _store_row(k, aid: str, row: dict) -> None:
    """A row in the agent's own store, written as the agent."""
    with as_agent(k, aid):
        await core._save_history(aid, k, [row])


async def test_grep_numeric_query_and_empty_ids_follow_python(seeded_kernel):
    """`str(query or "")`: a NUMERIC query is its digits; an EMPTY `ids` list is
    falsy, so it means NO SCOPE."""
    k = seeded_kernel
    _ai(k, "kp_gnum")
    await _store_row(k, "kp_gnum", {"role": "user", "content": "order 586 shipped"})
    r = await _call(k, "kp_gnum", {"type": "grep", "query": 586, "ids": []})
    assert r["total"] == 1, r
    assert list(r)[:3] == ["matches", "total", "truncated"], r
    assert render_reply(r).startswith("<matches>"), render_reply(r)


async def test_history_negative_limit_is_an_empty_page(seeded_kernel):
    """A NEGATIVE `limit` is already satisfied by the empty page."""
    k = seeded_kernel
    _ai(k, "kp_hneg", _Script())
    await _call(k, "kp_hneg", {"type": "send", "text": "ping"})
    r = await _call(k, "kp_hneg", {"type": "history", "limit": -1})
    assert r["rows"] == [], r
    assert r["next"] is None, r
    assert r["total"] >= 2, r


async def test_the_history_digest_pages_forward(seeded_kernel):
    """`limit` cuts the page, `next` names where to resume, and an `after` the store
    does not know pages NOTHING rather than starting over."""
    k = seeded_kernel
    _ai(k, "kp_hpage", _Script())
    await _call(k, "kp_hpage", {"type": "send", "text": "ping"})
    first = await _call(k, "kp_hpage", {"type": "history", "limit": 1})
    assert len(first["rows"]) == 1, first
    nxt = first["next"]
    assert isinstance(nxt, int), f"a cursor: {first}"
    second = await _call(k, "kp_hpage", {"type": "history", "after": nxt})
    assert second["rows"][0]["id"] > nxt, "the second page resumes past the cursor"
    nowhere = await _call(k, "kp_hpage", {"type": "history", "after": 999_999})
    assert nowhere["rows"] == [], "an unknown cursor pages nothing, never the head"


async def test_a_null_context_strategy_is_reported_raw_and_refused(seeded_kernel):
    """Only an ABSENT key means `compact`: `context_status` echoes the raw null and the
    projection refuses it with the unknown-strategy text."""
    k = seeded_kernel
    _ai(k, "kp_nullstrat", _Script())
    await k.send(
        [
            {
                "target": "kernel_state",
                "payload": {
                    "type": "update_agent",
                    "id": "kp_nullstrat",
                    "context_strategy": None,
                },
            }
        ]
    )
    st = await _call(k, "kp_nullstrat", {"type": "context_status"})
    assert st["strategy"] is None, st
    rec = k.ctx.get("kp_nullstrat")
    assert "context_strategy" in rec and rec["context_strategy"] is None, rec
    msgs = [{"role": "system", "content": "s"}] + [
        {"role": "user" if i % 2 == 0 else "assistant", "content": "x" * 200}
        for i in range(20)
    ]
    cfg = core.Backend(sentence="s", make_provider=lambda *a: None, name=NAME)
    out = await core._project_context(
        {**rec, "context_window": 500, "output_reserve": 0},
        cfg,
        None,
        "kp_nullstrat",
        k,
        "cli",
        msgs,
        len(msgs) - 1,
    )
    assert isinstance(out, dict), out
    assert out["error"].startswith(f"{NAME}: unknown context_strategy None (valid: "), (
        out
    )


async def test_send_to_is_kept_raw_and_dropped_only_when_blank(seeded_kernel):
    """`send_to` is kept VERBATIM and dropped only when it is not a string or is blank
    after a strip: `' peer '` is NOT the agent `peer` — nothing is handed to it."""
    k = seeded_kernel
    handed: list[tuple[str, str]] = []

    async def spy(id, payload, kernel):
        if payload.get("type") == "send":
            handed.append((id, payload.get("text", "")))
        return None

    mod = types.ModuleType("kp_sendto_spy.tools")
    mod.handler = spy
    mod.VERBS = {}
    register_unlisted("kp_sendto_spy.tools", mod)
    k.create("kp_sendto_spy.tools", id="peer", callable_by={"kp_st": "*"})
    _ai(k, "kp_st", can_call={"peer": "*"})
    done = 'the answer <call name="done"></call>'

    async def run(send_to) -> list:
        handed.clear()
        _fakes["kp_st"] = _Script([done])
        await _call(k, "kp_st", {"type": "send", "text": "hi", "send_to": send_to})
        await asyncio.sleep(0.3)
        return list(handed)

    assert await run(" peer ") == [], "the raw ` peer ` is not the agent `peer`"
    assert await run("\n\t") == []
    assert await run(7) == []
    got = await run("peer")
    assert [h[0] for h in got] == ["peer"], got
    assert "the answer" in got[0][1], got
