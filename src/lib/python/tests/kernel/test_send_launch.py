"""The SEND LAUNCH PHASE, pinned item by item: every refusal's exact text and
`arg`, which items get a pid, and what the draining kernel still lets through.

`Agent._launch` validates an item in a fixed order and dispatches it down one of
five branches (named join, named kernel-reflect, named, unnamed kernel-reflect,
unnamed). Collapsing those (reduction candidate C) must keep each observable
outcome below — the refusal a caller reads, the `pid` that is or is not there,
and the order the checks run in (the first failing check is the one reported).
"""

from __future__ import annotations

import asyncio
import types

from kernel import register_unlisted

from _testkit import as_agent

_reached: list = []
_go: dict = {}


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "launch probe", "verbs": {}}
    if verb == "relay":
        # a send made from INSIDE a live dispatch (after the test says go)
        _go.setdefault("started", asyncio.Event()).set()
        await _go.setdefault("go", asyncio.Event()).wait()
        return (
            await kernel.send([{"target": payload["to"], "payload": {"type": "work"}}])
        )[0]
    _reached.append((id, verb))
    return {"ok": id}


_mod = types.ModuleType("launch_probe.tools")
_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("launch_probe.tools", _mod)


def _mk(k, aid: str, **meta) -> str:
    rec = k.create("launch_probe.tools", id=aid, **meta)
    assert rec.get("id") == aid, rec
    return aid


async def _one(k, item) -> dict:
    return (await k.send([item]))[0]


def _refused(out: dict) -> dict:
    assert out["pid"] is None and out["status"] == "failed", out
    return out["result"]


# ─── each refusal, exactly ───────────────────────────────────────────────────


async def test_an_item_that_is_not_a_dict(seeded_kernel):
    r = _refused(await _one(seeded_kernel, ["kernel", {"type": "ps"}]))
    assert r == {
        "error": "send: item must be a dict",
        "reason": "bad_arg",
        "arg": "item",
    }


async def test_empty_target_is_missing_arg(seeded_kernel):
    for target in (None, "", 7):
        r = _refused(
            await _one(seeded_kernel, {"target": target, "payload": {"type": "ps"}})
        )
        assert r == {
            "error": "send: target required",
            "reason": "missing_arg",
            "arg": "target",
        }, target


async def test_a_payload_that_is_not_a_dict(seeded_kernel):
    for payload in (None, "ps", ["ps"]):
        r = _refused(
            await _one(seeded_kernel, {"target": "kernel", "payload": payload})
        )
        assert r == {
            "error": "send: payload required",
            "reason": "missing_arg",
            "arg": "payload",
        }, payload


async def test_a_bad_timeout(seeded_kernel):
    for t in (0, -1, True, "5"):
        r = _refused(
            await _one(
                seeded_kernel,
                {"target": "kernel", "payload": {"type": "ps"}, "timeout": t},
            )
        )
        assert r == {
            "error": f"send: timeout must be a positive number of seconds, got {t!r}",
            "reason": "bad_arg",
            "arg": "timeout",
        }, t
# twin: non_string_type_is_bad_arg
# twin: a_body_sent_as_type_is_refused_by_its_type


async def test_a_missing_and_a_bad_verb(seeded_kernel):
    r = _refused(await _one(seeded_kernel, {"target": "kernel", "payload": {}}))
    assert r == {
        "error": "send: payload.type required",
        "reason": "missing_arg",
        "arg": "payload.type",
    }
    # A non-string is named by its TYPE, never echoed — a body sent as `type`
    # would come back whole in the refusal, several times its size.
    for verb, got in (
        ("", "''"),
        (None, "NoneType"),
        (3, "int"),
        (["go"], "list"),
        (b"\x00" * 4096, "bytes"),
    ):
        r = _refused(
            await _one(seeded_kernel, {"target": "kernel", "payload": {"type": verb}})
        )
        assert r == {
            "error": f"send: payload.type must be a non-empty string, got {got}",
            "reason": "bad_arg",
            "arg": "payload.type",
        }, verb


async def test_empty_id_is_refused_bad_arg(seeded_kernel):
    item = {"target": "kernel", "payload": {"type": "ps"}}
    for bad in (True, 1.5, [1], {"a": 1}):
        r = _refused(await _one(seeded_kernel, {**item, "id": bad}))
        assert r == {
            "error": (
                "send: id must be a name (non-empty string) or a message id (int), "
                f"got {bad!r}"
            ),
            "reason": "bad_arg",
            "arg": "id",
        }, bad
    r = _refused(await _one(seeded_kernel, {**item, "id": ""}))
    assert r == {
        "error": "send: id must be a non-empty name",
        "reason": "bad_arg",
        "arg": "id",
    }
    r = _refused(await _one(seeded_kernel, {**item, "id": 987654}))
    assert r == {
        "error": (
            "send: id 987654 names no known execution — ids are minted by the "
            "kernel and can be joined, never created"
        ),
        "reason": "bad_arg",
        "arg": "id",
    }


async def test_bad_retries(seeded_kernel):
    item = {"target": "kernel", "payload": {"type": "ps"}}
    for bad in (-1, True, 1.0, "2"):
        for named in (False, True):
            it = {**item, "retries": bad, **({"id": "rt"} if named else {})}
            r = _refused(await _one(seeded_kernel, it))
            assert r == {
                "error": f"send: retries must be a non-negative int, got {bad!r}",
                "reason": "bad_arg",
                "arg": "retries",
            }, (bad, named)


async def test_an_int_id_rebind_renders_bare_digits(seeded_kernel):
    k = seeded_kernel
    _mk(k, "first")
    _mk(k, "second")
    ok = await _one(k, {"target": "first", "payload": {"type": "work"}, "id": "n1"})
    assert ok["status"] == "completed" and ok["name"] == "n1"
    r = _refused(
        await _one(k, {"target": "second", "payload": {"type": "work"}, "id": "n1"})
    )
    assert r == {
        "error": (
            "send: id 'n1' already names an execution on 'first', not 'second' — "
            "an id is bound to its first execution; a retry aimed elsewhere needs "
            "a new id"
        ),
        "reason": "bad_arg",
        "arg": "id",
    }
    # the same binding holds when the execution is joined by its int id
    r = _refused(
        await _one(
            k, {"target": "second", "payload": {"type": "work"}, "id": ok["pid"]}
        )
    )
    assert r["error"] == (
        f"send: id {ok['pid']} already names an execution on 'first', not "
        "'second' — an id is bound to its first execution; a retry aimed "
        "elsewhere needs a new id"
    )


async def test_an_unknown_target_lists_nothing_it_should_not(seeded_kernel):
    r = _refused(
        await _one(seeded_kernel, {"target": "nobody", "payload": {"type": "ps"}})
    )
    assert r["reason"] == "unknown_agent" and "nobody" in r["error"], r


async def test_the_first_failing_check_is_the_one_reported(seeded_kernel):
    # an unknown target AND a bad timeout AND a bad id: the timeout is checked
    # first (item fields before resolution), then payload.type, then id
    r = _refused(
        await _one(
            seeded_kernel,
            {"target": "nobody", "payload": {}, "timeout": -1, "id": True},
        )
    )
    assert r["arg"] == "timeout"
    r = _refused(
        await _one(seeded_kernel, {"target": "nobody", "payload": {}, "id": True})
    )
    assert r["arg"] == "payload.type"
    r = _refused(
        await _one(
            seeded_kernel,
            {"target": "nobody", "payload": {"type": "x"}, "id": True, "retries": -1},
        )
    )
    assert r["arg"] == "id"
    r = _refused(
        await _one(
            seeded_kernel, {"target": "nobody", "payload": {"type": "x"}, "retries": -1}
        )
    )
    assert r["arg"] == "retries"


# ─── which items carry a pid ─────────────────────────────────────────────────


async def test_pids_across_the_five_launch_branches(seeded_kernel):
    k = seeded_kernel
    _mk(k, "w")
    outs = await k.send(
        [
            {"target": "kernel", "payload": {"type": "reflect"}},  # unnamed fast
            {
                "target": "kernel",
                "payload": {"type": "reflect"},
                "id": "fr",
            },  # named fast
            {"target": "w", "payload": {"type": "work"}},  # unnamed
            {"target": "w", "payload": {"type": "work"}, "id": "nw"},  # named
            {"target": "nobody", "payload": {"type": "work"}},  # refusal
        ]
    )
    assert [o["status"] for o in outs] == [
        "completed",
        "completed",
        "completed",
        "completed",
        "failed",
    ]
    assert outs[0]["pid"] is None and "name" not in outs[0]
    assert isinstance(outs[1]["pid"], int) and outs[1]["name"] == "fr"
    assert isinstance(outs[2]["pid"], int) and "name" not in outs[2]
    assert isinstance(outs[3]["pid"], int) and outs[3]["name"] == "nw"
    assert outs[4]["pid"] is None
    assert len({outs[1]["pid"], outs[2]["pid"], outs[3]["pid"]}) == 3
    # a named join (string or int) hands back the SAME outcome, no second run
    _reached.clear()
    again = await k.send(
        [
            {"target": "w", "payload": {"type": "work"}, "id": "nw"},
            {"target": "w", "payload": {"type": "work"}, "id": outs[2]["pid"]},
        ]
    )
    assert again[0] == outs[3]
    assert again[1]["pid"] == outs[2]["pid"] and again[1]["result"] == outs[2]["result"]
    assert _reached == []


async def test_an_int_id_joins_and_never_creates(seeded_kernel):
    k = seeded_kernel
    _mk(k, "w2")
    first = await _one(k, {"target": "w2", "payload": {"type": "work"}})
    joined = await _one(
        k, {"target": "w2", "payload": {"type": "anything"}, "id": first["pid"]}
    )
    # the joiner reads the ORIGINAL execution's outcome, whatever it sent
    assert joined["pid"] == first["pid"] and joined["result"] == first["result"]
    assert "name" not in joined


async def test_a_named_join_is_gated_on_the_original_verb(seeded_kernel):
    k = seeded_kernel
    _mk(k, "svc", callable_by={"j": ["safe"]})
    _mk(k, "j", can_call={"svc": ["safe"]})
    await _one(k, {"target": "svc", "payload": {"type": "secret_verb"}, "id": "gj"})
    with as_agent(seeded_kernel, "j"):
        r = _refused(
            await _one(k, {"target": "svc", "payload": {"type": "safe"}, "id": "gj"})
        )
    assert r["error"] == "j may not call 'secret_verb' on svc"
    assert r["side"] == "egress"


# ─── the draining kernel ─────────────────────────────────────────────────────


async def test_while_draining_shutdown_traffic_and_live_work_pass_and_new_roots_do_not(seeded_kernel):
    k = seeded_kernel
    _mk(k, "d1")
    k.ctx.draining = True
    try:
        r = _refused(await _one(k, {"target": "d1", "payload": {"type": "work"}}))
        assert r == {
            "error": (
                "send: the kernel is draining for shutdown — no new work is "
                "accepted; in-flight work may finish"
            ),
            "reason": "draining",
        }
        # the root's control surface stays open: kernel verbs and reflect
        for target in ("kernel", k.id):
            for verb in ("ps", "reflect"):
                out = await _one(k, {"target": target, "payload": {"type": verb}})
                assert out["status"] == "completed", (target, verb, out)
        # a non-system, non-reflect verb to the root is a new root: refused
        r = _refused(await _one(k, {"target": "kernel", "payload": {"type": "boot"}}))
        assert r["reason"] == "draining"
        # a reflect of a non-root agent is a new root too
        r = _refused(await _one(k, {"target": "d1", "payload": {"type": "reflect"}}))
        assert r["reason"] == "draining"
    finally:
        k.ctx.draining = False


async def test_draining_lets_a_live_dispatch_send(seeded_kernel):
    k = seeded_kernel
    _mk(k, "relay", can_call={"sink": ["work"]})
    _mk(k, "sink", callable_by={"relay": ["work"]})
    _go.clear()
    started = _go.setdefault("started", asyncio.Event())
    go = _go.setdefault("go", asyncio.Event())
    pending = asyncio.create_task(
        k.send([{"target": "relay", "payload": {"type": "relay", "to": "sink"}}])
    )
    await asyncio.wait_for(started.wait(), timeout=2)
    k.ctx.draining = True  # the relay is live work; its own send must pass
    try:
        go.set()
        out = (await asyncio.wait_for(pending, timeout=2))[0]
    finally:
        k.ctx.draining = False
    assert out["status"] == "completed", out
    inner = out["result"]
    assert inner["status"] == "completed" and inner["result"] == {"ok": "sink"}, inner
    assert isinstance(inner["pid"], int)


async def test_while_draining_a_fork_is_refused_and_a_keepalive_fork_is_not(seeded_kernel):
    """A FORK (a ws call, a scheduler tick, a bridge read loop) starting a send
    while draining is new work: refused. A keepalive fork — a persister that must
    outlive the quiet phase — still sends."""
    k = seeded_kernel
    _mk(k, "d2")

    async def fork_send():
        return await _one(k, {"target": "d2", "payload": {"type": "work"}})

    k.ctx.draining = True
    try:
        for keepalive, expect in ((False, "failed"), (True, "completed")):
            tid = k.ctx.spawn_task("d2", "loop", fork_send(), keepalive=keepalive)
            out = await asyncio.wait_for(k.ctx.tasks[tid]["_task"], timeout=2)
            assert out["status"] == expect, (keepalive, out)
            if expect == "failed":
                assert out["result"]["reason"] == "draining"
    finally:
        k.ctx.draining = False
