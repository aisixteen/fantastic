"""The BATCH SEND — array-only `send([...]) -> [{pid, status, result}]`.

One shape, no single-call sugar: its absence is what prevents single/batch
drift. Dispatch is CONCURRENT, the returned array preserves input order, and
`timeout` is per-item — the kernel NEVER defaults it (absent = wait forever).
Expiry goes through the FRONT DOOR (`cancel_task`), so every bundle's
`on_cancel` hook runs before the abort.

Statuses: completed | errored | failed | cancelled | timed_out.
Invariant: `pid is None` ⟺ the item never reached the task table.
"""

from __future__ import annotations

import asyncio
import time
import types

from kernel._kernel import Kernel
from kernel import register_unlisted
from _testkit import as_agent, wire

# A SELF-OWNED slow bundle. Importing test_task_table's would share its module
# state, and under xdist the import can be a SECOND instance of that module —
# run flags and hook spies then point at dicts the injected handler never
# touches, and every wait times out.

_started: dict[str, asyncio.Event] = {}
_hook_calls: list[tuple[str, str, bool]] = []


def _run_flag(agent_id: str) -> asyncio.Event:
    return _started.setdefault(agent_id, asyncio.Event())


async def _bslow_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "batch slow test agent", "verbs": {}}
    if verb == "slow":
        _run_flag(id).set()
        await asyncio.sleep(30)
        return {"finished": True}
    if verb == "chain":
        return (
            await kernel.send(
                [{"target": payload["next"], "payload": {"type": "slow"}}]
            )
        )[0]["result"]
    return {"error": f"unknown verb {verb!r}"}


async def _bslow_on_cancel(id, task_id, kernel):
    entry = kernel.ctx.tasks.get(task_id)
    alive = entry is not None and not entry["_task"].done()
    _hook_calls.append((id, task_id, alive))


_mod = types.ModuleType("batch_slow_agent.tools")
_mod.handler = _bslow_handler
_mod.on_cancel = _bslow_on_cancel
_mod.VERBS = {}
register_unlisted("batch_slow_agent.tools", _mod)

SLOW = "batch_slow_agent.tools"


async def _mk(kernel, agent_id: str, **grants) -> str:
    payload = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "handler_module": SLOW,
        "id": agent_id,
        **grants,
    }
    rec = (await kernel.send([{"target": "kernel_state", "payload": payload}]))[0][
        "result"
    ]
    return rec["id"]


async def _rows(kernel) -> list[dict]:
    r = (await kernel.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0]
    return r["result"]["tasks"]


# ─── shape and order ────────────────────────────────────────────


async def test_mixed_batch_settles_in_input_order(seeded_kernel):
    """Refusals (no pid) and dispatches (real pid) come back in the slots the
    caller wrote them, however they finished."""
    a = await _mk(seeded_kernel, "mix_a")
    outs = await seeded_kernel.send(
        [
            {"target": "nobody_home", "payload": {"type": "reflect"}},
            {"target": a, "payload": {"type": "reflect"}},
            {"target": a, "payload": None},  # malformed: no payload
            {"target": "kernel", "payload": {"type": "list_agents"}},
        ]
    )
    assert [o["status"] for o in outs] == ["failed", "completed", "failed", "completed"]
    assert outs[0]["pid"] is None and outs[0]["result"]["reason"] == "unknown_agent"
    assert isinstance(outs[1]["pid"], int) and outs[1]["pid"] > 0
    assert outs[2]["pid"] is None and outs[2]["result"]["reason"] == "missing_arg"
    assert "agents" in outs[3]["result"]


async def test_gate_denial_is_a_failed_outcome_with_no_pid(seeded_kernel):
    """A two-sided-gate denial never reaches the task table; the refusal dict
    rides in `result` unchanged (same shape callers always branched on)."""
    from _testkit import as_agent

    await _mk(seeded_kernel, "gated_a")  # deny-all: no can_call grants
    await _mk(seeded_kernel, "gated_b")  # sibling — outside the $parent baseline
    with as_agent(seeded_kernel, "gated_a"):
        out = (
            await seeded_kernel.send(
                [{"target": "gated_b", "payload": {"type": "reflect"}}]
            )
        )[0]
    assert out["status"] == "failed" and out["pid"] is None
    assert out["result"]["reason"] == "unauthorized"


async def test_non_list_argument_is_a_loud_typeerror(seeded_kernel):
    """The migration tripwire: the old (target, payload) form fails at the
    call, not deep in a handler."""
    try:
        await seeded_kernel.send("kernel")
    except TypeError as e:
        assert "LIST" in str(e)
    else:
        raise AssertionError("a non-list send must not be accepted")
    try:
        await seeded_kernel.send("kernel", {"type": "ps"})
    except TypeError:
        pass  # Python's own arity error — loud is what matters
    else:
        raise AssertionError("two-positional send must not be accepted")


# ─── concurrency ────────────────────────────────────────────────


async def test_batch_dispatch_is_concurrent_not_serial(seeded_kernel):
    """Two slow items: both rows in `ps` mid-flight, wall ≈ max, not sum."""
    a = await _mk(seeded_kernel, "conc_a")
    b = await _mk(seeded_kernel, "conc_b")
    batch = asyncio.create_task(
        seeded_kernel.send(
            [
                {"target": a, "payload": {"type": "slow"}, "timeout": 0.4},
                {"target": b, "payload": {"type": "slow"}, "timeout": 0.4},
            ]
        )
    )
    await asyncio.wait_for(_run_flag(a).wait(), timeout=2)
    await asyncio.wait_for(_run_flag(b).wait(), timeout=2)  # ← both live AT ONCE
    rows = await _rows(seeded_kernel)
    assert {t["agent"] for t in rows if t["verb"] == "slow"} == {a, b}
    t0 = time.monotonic()
    outs = await batch
    assert time.monotonic() - t0 < 2, "batch must settle ~max(item), not sum"
    assert [o["status"] for o in outs] == ["timed_out", "timed_out"]


async def test_two_items_to_one_agent_get_two_pids(seeded_kernel):
    """The substrate does not serialize per agent — any FIFO is a bundle's own
    lock. Two dispatches, two distinct pids, both complete."""
    a = await _mk(seeded_kernel, "twin_a")
    outs = await seeded_kernel.send(
        [
            {"target": a, "payload": {"type": "reflect"}},
            {"target": a, "payload": {"type": "reflect"}},
        ]
    )
    assert [o["status"] for o in outs] == ["completed", "completed"]
    assert outs[0]["pid"] != outs[1]["pid"]


# ─── timeout: the front door ────────────────────────────────────


async def test_timeout_kills_through_the_front_door(seeded_kernel):
    """Expiry runs the bundle's `on_cancel` BEFORE the abort (the spy sees its
    task still alive), the row leaves the table, and the outcome names the
    deadline it enforced."""
    _hook_calls.clear()
    a = await _mk(seeded_kernel, "slowpoke")
    out = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "slow"}, "timeout": 0.1}]
        )
    )[0]
    assert out["status"] == "timed_out"
    assert out["result"]["reason"] == "timeout"
    assert "0.1" in out["result"]["error"]
    assert out["pid"] and out["result"]["task_id"] == out["pid"]
    assert [(aid, alive) for aid, _tid, alive in _hook_calls] == [(a, True)]
    assert all(t["agent"] != a for t in await _rows(seeded_kernel))


async def test_timeout_kills_the_subtree_too(seeded_kernel):
    """A timed-out `chain` takes its nested `slow` child down with it — the
    same leaves-up walk `cancel` always did."""
    _hook_calls.clear()
    a = await _mk(seeded_kernel, "chain_to", can_call={"leaf_to": ["slow"]})
    b = await _mk(seeded_kernel, "leaf_to", callable_by={"chain_to": ["slow"]})
    out = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "chain", "next": b}, "timeout": 0.2}]
        )
    )[0]
    assert out["status"] == "timed_out"
    hooked = {aid for aid, _tid, _alive in _hook_calls}
    assert hooked == {a, b}, f"subtree hooks: {hooked}"
    assert all(t["agent"] not in (a, b) for t in await _rows(seeded_kernel))


async def test_no_timeout_waits_out_a_slow_handler(seeded_kernel):
    """Absent timeout = wait forever. A handler slower than any would-be
    default still completes."""
    mod = types.ModuleType("dawdle_agent.tools")

    async def handler(id, payload, kernel):
        if payload.get("type") == "dawdle":
            await asyncio.sleep(0.3)
            return {"took": "its time"}
        return None

    mod.handler = handler
    mod.VERBS = {}
    register_unlisted("dawdle_agent.tools", mod)
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "handler_module": "dawdle_agent.tools",
                        "id": "dawdler",
                    },
                }
            ]
        )
    )[0]["result"]
    out = (
        await seeded_kernel.send([{"target": rec["id"], "payload": {"type": "dawdle"}}])
    )[0]
    assert out["status"] == "completed" and out["result"] == {"took": "its time"}


async def test_bad_timeout_is_refused_before_dispatch(seeded_kernel):
    a = await _mk(seeded_kernel, "t_bad")
    for bad in (0, -1, "5", True):
        out = (
            await seeded_kernel.send(
                [{"target": a, "payload": {"type": "reflect"}, "timeout": bad}]
            )
        )[0]
        assert out["status"] == "failed" and out["pid"] is None, bad
        assert out["result"]["reason"] == "bad_arg"


# ─── cancellation ───────────────────────────────────────────────


async def test_third_party_cancel_settles_one_item_only(seeded_kernel):
    """`cancel` of one item's pid → that slot is `cancelled` with the exact
    blocked-caller shape; the sibling completes untouched."""
    a = await _mk(seeded_kernel, "cx_a")
    b = await _mk(seeded_kernel, "cx_b")
    batch = asyncio.create_task(
        seeded_kernel.send(
            [
                {"target": a, "payload": {"type": "slow"}},
                {"target": b, "payload": {"type": "reflect"}},
            ]
        )
    )
    await asyncio.wait_for(_run_flag(a).wait(), timeout=2)
    row = next(t for t in await _rows(seeded_kernel) if t["agent"] == a)
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "cancel", "task_id": row["id"]}}]
        )
    )[0]["result"]
    assert r["cancelled"] is True
    outs = await asyncio.wait_for(batch, timeout=2)
    assert outs[0]["status"] == "cancelled"
    assert outs[0]["result"] == {"cancelled": True}
    assert outs[1]["status"] == "completed"


async def test_cancelled_caller_takes_the_batch_down(seeded_kernel):
    """Killing the caller's task propagates CancelledError out of `send` and
    drains every dispatched row — children die with the batch."""
    a = await _mk(seeded_kernel, "cc_a")
    b = await _mk(seeded_kernel, "cc_b")
    batch = asyncio.create_task(
        seeded_kernel.send(
            [
                {"target": a, "payload": {"type": "slow"}},
                {"target": b, "payload": {"type": "slow"}},
            ]
        )
    )
    await asyncio.wait_for(_run_flag(a).wait(), timeout=2)
    await asyncio.wait_for(_run_flag(b).wait(), timeout=2)
    batch.cancel()
    try:
        await batch
        raise AssertionError("cancelled batch must raise")
    except asyncio.CancelledError:
        pass
    await asyncio.sleep(0)  # let the aborts land
    assert all(t["agent"] not in (a, b) for t in await _rows(seeded_kernel))


# ─── failure isolation ─────────────────────────────────────────


async def test_handler_raise_settles_as_failed_and_send_never_raises(seeded_kernel):
    mod = types.ModuleType("raiser_agent.tools")

    async def handler(id, payload, kernel):
        if payload.get("type") == "boom":
            raise RuntimeError("kaput")
        return None

    mod.handler = handler
    mod.VERBS = {}
    register_unlisted("raiser_agent.tools", mod)
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "handler_module": "raiser_agent.tools",
                        "id": "raiser",
                    },
                }
            ]
        )
    )[0]["result"]
    outs = await seeded_kernel.send(
        [
            {"target": rec["id"], "payload": {"type": "boom"}},
            {"target": "kernel", "payload": {"type": "ps"}},
        ]
    )
    assert outs[0]["status"] == "failed"
    assert outs[0]["result"]["raised"] is True
    assert "RuntimeError: kaput" in outs[0]["result"]["error"]
    assert outs[1]["status"] == "completed", "a sibling must not be poisoned"


async def test_a_handler_sys_exit_settles_failed_and_the_loop_runs_on(seeded_kernel):
    """A SystemExit out of an asyncio task stops the EVENT LOOP — the whole
    kernel, for one handler that called sys.exit.
    It is a raise like any other: that call settles failed, a sibling and the
    next call complete."""
    mod = types.ModuleType("exiter_agent.tools")

    async def handler(id, payload, kernel):
        if payload.get("type") == "quit":
            raise SystemExit(3)
        return None

    mod.handler = handler
    register_unlisted("exiter_agent.tools", mod)
    seeded_kernel.create("exiter_agent.tools", id="exiter")
    outs = await seeded_kernel.send(
        [
            {"target": "exiter", "payload": {"type": "quit"}},
            {"target": "kernel", "payload": {"type": "ps"}},
        ]
    )
    assert outs[0]["status"] == "failed", outs[0]
    assert outs[0]["result"] == {"error": "SystemExit: 3", "raised": True}
    assert outs[1]["status"] == "completed", "a sibling must not be poisoned"
    again = await seeded_kernel.send([{"target": "kernel", "payload": {"type": "ps"}}])
    assert again[0]["status"] == "completed"


async def test_a_module_whose_top_level_exits_is_never_imported(
    seeded_kernel, tmp_path, monkeypatch
):
    """Importing ANY handler_module on `sys.path` to validate its meta would let
    one call naming a module whose top level exits (stdlib `unittest.__main__` is
    one) take the kernel down — any agent may create under ITSELF on its baseline.
    A name outside the bundle catalog is never imported: the create keeps the record as one this
    runtime cannot answer (as rust does), the call to it is refused, and
    the kernel answers the next call."""
    from _testkit import as_agent

    (tmp_path / "arch21_exit_mod.py").write_text("raise SystemExit(7)\n")
    monkeypatch.syspath_prepend(str(tmp_path))
    seeded_kernel.create(None, id="arch21_worker")
    wire(seeded_kernel, "arch21_worker", "kernel_state", ["create_agent"])
    with as_agent(seeded_kernel, "arch21_worker"):
        out = (
            await seeded_kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": "arch21_worker",
                            "handler_module": "arch21_exit_mod",
                            "id": "arch21_child",
                        },
                    }
                ]
            )
        )[0]
    assert out["status"] == "completed", out
    assert "arch21_child" in seeded_kernel.ctx.agents
    called = (
        await seeded_kernel.send(
            [{"target": "arch21_child", "payload": {"type": "ping"}}]
        )
    )[0]
    assert called["result"] == {
        "error": (
            "agent 'arch21_child': handler_module 'arch21_exit_mod' cannot be "
            "imported in this runtime: ModuleNotFoundError: No module named "
            "'arch21_exit_mod'"
        ),
        "reason": "unhandled",
        "verb": "ping",
    }
    again = await seeded_kernel.send([{"target": "kernel", "payload": {"type": "ps"}}])
    assert again[0]["status"] == "completed"


# ─── the fast path and the Kernel facade ───────────────────────


async def test_kernel_reflect_fast_path_mints_no_row(seeded_kernel):
    """`kernel`+`reflect` is a pure identity read: completed, `pid is None`,
    and it never appears in the table (assert from INSIDE the batch, next to a
    live row that proves ps ran mid-flight)."""
    a = await _mk(seeded_kernel, "fp_a")
    batch = asyncio.create_task(
        seeded_kernel.send(
            [
                {"target": a, "payload": {"type": "slow"}, "timeout": 0.3},
                {"target": "kernel", "payload": {"type": "reflect"}},
            ]
        )
    )
    await asyncio.wait_for(_run_flag(a).wait(), timeout=2)
    rows = await _rows(seeded_kernel)
    assert all(t["verb"] != "reflect" or t["agent"] != "kernel" for t in rows)
    outs = await batch
    assert outs[1]["status"] == "completed" and outs[1]["pid"] is None
    assert outs[1]["result"]["id"] == seeded_kernel.id


async def test_kernel_facade_with_no_root_fails_per_item():
    k = Kernel()
    outs = await k.send(
        [
            {"target": "x", "payload": {"type": "reflect"}},
            {"target": "y", "payload": {"type": "reflect"}},
        ]
    )
    assert [o["status"] for o in outs] == ["failed", "failed"]
    assert all(o["pid"] is None and "no root" in o["result"]["error"] for o in outs)


# ─── parallel EXECUTION holds; ARRIVAL is in input order ─────────────────────

_ov_started: dict[str, asyncio.Event] = {}
_ov_arrivals: list[str] = []


async def _overlap_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown", "reflect"):
        return {"id": id, "sentence": "overlap probe", "verbs": {}}
    me = payload["me"]
    _ov_arrivals.append(me)
    _ov_started.setdefault(me, asyncio.Event()).set()
    if other := payload.get("wait_for"):
        # holds until the OTHER item has STARTED: serialized execution never gets there
        ev = _ov_started.setdefault(other, asyncio.Event())
        try:
            await asyncio.wait_for(ev.wait(), 3.0)
        except asyncio.TimeoutError:
            return {"overlapped": False}
    return {"overlapped": True}


_ov = types.ModuleType("overlap_agent.tools")
_ov.handler = _overlap_handler
_ov.VERBS = {}
register_unlisted("overlap_agent.tools", _ov)


async def test_a_batch_executes_its_items_in_parallel_and_delivers_them_in_order(
    seeded_kernel,
):
    """Item A waits until item B has STARTED, so A completes only if both run AT
    ONCE — to one agent and to two. A reaches its target before B (input order)."""
    k = seeded_kernel
    for aid in ("ov_one", "ov_two"):
        await k.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "handler_module": "overlap_agent.tools",
                        "id": aid,
                    },
                }
            ]
        )
    for ta, tb in (("ov_one", "ov_one"), ("ov_one", "ov_two")):
        _ov_started.clear()
        _ov_arrivals.clear()
        outs = await k.send(
            [
                {"target": ta, "payload": {"type": "go", "me": "A", "wait_for": "B"}},
                {"target": tb, "payload": {"type": "go", "me": "B"}},
            ]
        )
        assert [o["status"] for o in outs] == ["completed", "completed"], outs
        assert outs[0]["result"] == {"overlapped": True}, (ta, tb, outs)
        assert _ov_arrivals == ["A", "B"], (ta, tb, _ov_arrivals)


# Rust twin: fantastic-kernel src/send/batch_tests.rs::the_whole_batch_is_visible_in_ps_before_the_first_await
async def test_the_whole_batch_is_visible_in_ps_before_the_first_await(seeded_kernel):
    # Pids are minted in a SYNCHRONOUS launch phase: were they minted as each
    # item awaited, `ps` would show the batch appearing one row at a time.
    a = await _mk(seeded_kernel, "b_ps")
    batch = asyncio.create_task(
        seeded_kernel.send(
            [
                {"target": a, "payload": {"type": "slow"}, "timeout": 1.0},
                {"target": a, "payload": {"type": "slow"}, "timeout": 1.0},
            ]
        )
    )
    # ONE yield: the batch task runs its synchronous launch phase and parks on
    # the gather; no timer elapses, so nothing can have been minted "later".
    await asyncio.sleep(0)
    rows = len(
        [
            r
            for r in await _rows(seeded_kernel)
            if r.get("agent") == a and r.get("verb") == "slow"
        ]
    )
    assert rows == 2, "the batch did not appear whole in ps"
    await batch


# ─── Rust twins (#170): each `test_<rust test name>` asserts what the Rust test does ─

_twin_runs: list[str] = []


async def _twin_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        if id in _slow_reflect:
            await asyncio.sleep(_slow_reflect[id])
        return {"id": id, "sentence": "batch twin probe", "verbs": {}}
    if verb == "fast":
        return {"ok": True}
    if verb == "slow":
        await asyncio.sleep(30)
        return {"finished": True}
    if verb == "raised_reply":
        _twin_runs.append(id)
        return {"error": "relayed peer failure", "raised": True}
    return {"error": f"unknown verb {verb!r}"}


_slow_reflect: dict[str, float] = {}
_twin_mod = types.ModuleType("batch_twin_probe.tools")
_twin_mod.handler = _twin_handler
_twin_mod.VERBS = {}
register_unlisted("batch_twin_probe.tools", _twin_mod)


async def _twin_mk(kernel, agent_id: str, **meta) -> str:
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "batch_twin_probe.tools",
                        "id": agent_id,
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == agent_id, rec
    return agent_id


async def test_a_name_that_looks_like_an_old_pid_is_just_a_name(seeded_kernel):
    """THERE IS NO PID NAMESPACE: a caller supplies only NAMES, so an id shaped
    `<agent>:<hex>` collides with nothing and is refused nothing -- an ordinary named
    execution that completes with a minted pid."""
    a = await _twin_mk(seeded_kernel, "b_ns")
    out = await seeded_kernel.send(
        [{"target": a, "payload": {"type": "fast"}, "id": "b_ns:deadbeef"}]
    )
    assert out[0]["status"] == "completed", out
    assert out[0]["name"] == "b_ns:deadbeef"
    assert isinstance(out[0]["pid"], int) and out[0]["pid"] > 0


async def test_an_empty_batch_is_an_empty_answer(seeded_kernel):
    assert await seeded_kernel.send([]) == []


async def test_a_returned_raised_dict_settles_errored_not_failed(seeded_kernel):
    """FAILED only when the handler RAISES; a returned dict -- even
    `{error, raised: true}`, a relayed peer's answer -- settles ERRORED: not retried,
    not a dead letter."""
    _twin_runs.clear()
    a = await _twin_mk(seeded_kernel, "b_raised")
    out = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "raised_reply"}, "retries": 2}]
        )
    )[0]
    assert out["status"] == "errored", out
    assert out["result"]["raised"] is True
    assert len(_twin_runs) == 1, "an errored reply is never retried"
    assert seeded_kernel.ctx.executions[out["pid"]]["attempts"] == 1


async def test_the_unnamed_kernel_reflect_honours_its_timeout(seeded_kernel):
    """Even the pid-less identity read runs under the deadline: on expiry it is
    cancelled and settles `timed_out` with `task_id: "-"`."""
    await _twin_mk(seeded_kernel, "b_slow_reflect")
    _slow_reflect["b_slow_reflect"] = 5
    try:
        t0 = time.monotonic()
        out = (
            await seeded_kernel.send(
                [{"target": "kernel", "payload": {"type": "reflect"}, "timeout": 0.1}]
            )
        )[0]
    finally:
        _slow_reflect.pop("b_slow_reflect", None)
    assert time.monotonic() - t0 < 3, "the deadline was ignored"
    assert out["status"] == "timed_out", out
    assert out["pid"] is None
    assert out["result"]["reason"] == "timeout"
    assert out["result"]["task_id"] == "-"
    assert out["result"]["error"] == (
        "send: kernel_state 'reflect' exceeded its 0.1s timeout; task cancelled"
    )


async def test_timeout_prose_and_row_keep_the_number_the_caller_gave(seeded_kernel):
    """`f"{timeout}s"` keeps the caller's number: an int prints `1s`, a float `0.1s`
    / `1e-07s`; the call row stores the value as given."""
    a = await _twin_mk(seeded_kernel, "b_prose")
    out = await seeded_kernel.send(
        [
            {"target": a, "payload": {"type": "slow"}, "timeout": 1},
            {"target": a, "payload": {"type": "slow"}, "timeout": 1e-7},
            {"target": a, "payload": {"type": "slow"}, "timeout": 0.1},
        ]
    )
    text = [o["result"]["error"] for o in out]
    assert text[0] == "send: b_prose 'slow' exceeded its 1s timeout; task cancelled"
    assert text[1] == "send: b_prose 'slow' exceeded its 1e-07s timeout; task cancelled"
    assert text[2] == "send: b_prose 'slow' exceeded its 0.1s timeout; task cancelled"
    row = seeded_kernel.ctx.rows[out[0]["pid"]]
    assert row["timeout"] == 1 and isinstance(row["timeout"], int), row
    assert seeded_kernel.ctx.rows[out[1]["pid"]]["timeout"] == 1e-7


async def test_a_dead_letter_of_a_departed_caller_is_announced_on_the_root(
    seeded_kernel,
):
    """`announce_deadletter` fans out directly, never gated; when the caller has left
    the tree the src falls back to the root, so a watcher of the root still hears it."""
    k = seeded_kernel
    caller = await _twin_mk(k, "dl_caller")
    child = await _twin_mk(k, "dl_child")
    wire(k, caller, child)
    inbox = k.ctx.ensure_inbox("tap_deadletter")
    assert k.watch(k.id, "tap_deadletter") is None
    with as_agent(k, caller):
        call = asyncio.create_task(
            k.send(
                [{"target": child, "payload": {"type": "slow"}, "timeout": 0.3}]
            )
        )
        await asyncio.sleep(0.05)
        # The caller leaves the index while its call is in flight.
        k.ctx.agents.pop(caller)
        out = (await call)[0]
    assert out["status"] == "timed_out", out
    announced = []
    while not inbox.empty():
        item = inbox.get_nowait()
        if item.get("type") == "on_deadletter":
            announced.append(item)
    assert announced, "the dead letter was announced on the root"
    assert announced[0]["caller"] == "dl_caller"
    assert announced[0]["status"] == "timed_out"
