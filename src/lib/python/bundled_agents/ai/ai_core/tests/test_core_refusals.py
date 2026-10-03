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
from kernel import sender_context, register_unlisted

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
    with sender_context(as_):
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
        "error": f"{NAME}: unknown type 'zzz'"
    }


# ─── inquiry ─────────────────────────────────────────────────────────────────


async def test_inquiry_texts(seeded_kernel, fake_providers):
    k = seeded_kernel
    _ai(k, "kp_iq0", _Script())
    for bad in ("", "   ", None, 5):
        assert await _call(k, "kp_iq0", {"type": "inquiry", "text": bad}) == {
            "error": "inquiry: `text` must be a non-empty string",
            "reason": "missing_arg",
            "arg": "text",
        }
    q = {"type": "inquiry", "text": "ship?", "request_id": "r1"}
    _ai(k, "kp_iq1", _Script(["I think yes."]))
    assert await _call(k, "kp_iq1", q) == {
        "error": (
            "you were sent an inquiry and did not answer it: emit "
            '<call name="inquiry_answer"><answer>what you concluded</answer></call>'
        ),
        "reason": "unhandled",
        "request_id": "r1",
    }
    _ai(k, "kp_iq2", _Script([render_call("inquiry_answer", {"answer": "..."})]))
    assert await _call(k, "kp_iq2", q) == {
        "error": (
            "that is the PLACEHOLDER, not an answer — `answer` came back as '...'. "
            "Say what you actually concluded."
        ),
        "reason": "bad_arg",
        "arg": "answer",
        "request_id": "r1",
    }
    _ai(k, "kp_iq3", _Script([render_call("inquiry_answer", {"answer": "ship it"})]))
    opts = {**q, "options": ["a", "b"], "action": {"type": "deploy"}}
    assert await _call(k, "kp_iq3", opts) == {"answer": "ship it", "request_id": "r1"}
    first_turn = fake_providers["kp_iq3"].seen[0]
    last_user = [m for m in first_turn if m["role"] == "user"][-1]
    assert last_user["content"].endswith(
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


async def test_fork_from_and_request_share_missing_args(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_f")
    base = {"type": "fork_from", "owner": "x", "grant": 1, "message": "go"}
    for name in ("owner", "message"):
        r = await _call(k, "kp_f", {**base, name: " "})
        assert r == {
            "error": f"fork_from: `{name}` is required",
            "reason": "missing_arg",
            "arg": name,
        }
    for g in (None, "x", True, 0):
        assert await _call(k, "kp_f", {**base, "grant": g}) == {
            "error": "fork_from: `grant` (an int) is required",
            "reason": "missing_arg",
            "arg": "grant",
        }, g
    assert await _call(k, "kp_f", {**base, "grant": 424242}) == {
        "error": "fork_from: no grant 424242 was given to you",
        "reason": "not_found",
        "arg": "grant",
    }
    for row in (None, "a", False):
        assert await _call(k, "kp_f", {"type": "request_share", "row": row}) == {
            "error": "request_share: `row` (an int) is required",
            "reason": "missing_arg",
            "arg": "row",
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


# ─── row events: a body only under a grant that covers the row ───────────────


async def _rows_seen(k, aid: str, share: dict, contents: list[str]) -> list:
    """`viewer` watches `aid` with body:true under the grant `share`; `aid` then
    appends `contents`. Returns, per row, whether its event carried the content."""
    k.create(None, id="viewer", can_call={aid: ["watch"], "memory": "*"})
    cb = dict(k.ctx.agents["memory"]._meta["callable_by"])
    k.ctx.update("memory", callable_by={**cb, "viewer": "*"})
    inbox = k.ctx.ensure_inbox("viewer")
    if share:
        g = await _call(
            k, "memory", {"type": "share", "with": "viewer", **share}, as_=aid
        )
        assert g.get("grant"), g
    await _call(k, aid, {"type": "watch", "body": True}, as_="viewer")
    out = []
    for c in contents:
        await core._save_history(aid, k, [{"role": "user", "content": c}])
        ev = inbox.get_nowait()
        assert ev["type"] == "row" and ev["source"] == aid
        out.append(ev["row"].get("content"))
    return out


async def _seed(k, aid: str, n: int) -> list[int]:
    await core._save_history(
        aid, k, [{"role": "user", "content": f"old {i}"} for i in range(n)]
    )
    rows = (await _call(k, "memory", {"type": "read"}, as_=aid))["rows"]
    return [r["id"] for r in rows]


async def test_a_frozen_head_grant_covers_only_rows_up_to_its_head(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_fz", callable_by={"viewer": ["watch"]})
    ids = await _seed(k, "kp_fz", 2)
    got = await _rows_seen(
        k, "kp_fz", {"scope": {"head_until": ids[-1]}, "mode": "frozen"}, ["new"]
    )
    assert got == [None]  # a row past a frozen head carries no body


async def test_a_live_head_grant_covers_new_rows(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_lv", callable_by={"viewer": ["watch"]})
    ids = await _seed(k, "kp_lv", 1)
    got = await _rows_seen(
        k, "kp_lv", {"scope": {"head_until": ids[-1]}, "mode": "live"}, ["new1", "new2"]
    )
    assert got == ["new1", "new2"]


async def test_a_row_grant_never_covers_a_new_row(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_rw", callable_by={"viewer": ["watch"]})
    ids = await _seed(k, "kp_rw", 1)
    got = await _rows_seen(
        k, "kp_rw", {"scope": {"row": ids[0]}, "mode": "live"}, ["n"]
    )
    assert got == [None]


async def test_no_grant_no_body(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_ng", callable_by={"viewer": ["watch"]})
    assert await _rows_seen(k, "kp_ng", {}, ["n"]) == [None]


async def test_a_range_grant_covers_new_rows_only_when_live(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_rg", callable_by={"viewer": ["watch"]})
    ids = await _seed(k, "kp_rg", 2)
    got = await _rows_seen(
        k,
        "kp_rg",
        {"scope": {"range": {"from": ids[0], "to": ids[1]}}, "mode": "live"},
        ["n1"],
    )
    assert got == ["n1"]


async def test_a_frozen_range_grant_stops_at_its_end(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_fr", callable_by={"viewer": ["watch"]})
    ids = await _seed(k, "kp_fr", 2)
    got = await _rows_seen(
        k,
        "kp_fr",
        {"scope": {"range": {"from": ids[0], "to": ids[1]}}, "mode": "frozen"},
        ["n1"],
    )
    assert got == [None]


async def test_a_fold_grant_covers_only_the_folds_closure(seeded_kernel):
    k = seeded_kernel
    _ai(k, "kp_fd", callable_by={"viewer": ["watch"]})
    ids = await _seed(k, "kp_fd", 2)
    fold = await _call(
        k,
        "memory",
        {
            "type": "append",
            "rows": [{"kind": "fold", "refs": ids, "title": "t", "content": "s"}],
        },
        as_="kp_fd",
    )
    assert fold.get("ids"), fold
    got = await _rows_seen(
        k, "kp_fd", {"scope": {"fold": fold["ids"][0]}, "mode": "live"}, ["n1"]
    )
    assert got == [None]  # a new row is not a member of the fold


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
