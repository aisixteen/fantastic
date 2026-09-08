"""The BATCH SEND — array-only `send([...]) -> [{pid, status, result}]`.

One shape, no single-call sugar: its absence is what prevents single/batch
drift. Dispatch is CONCURRENT, the returned array preserves input order, and
`timeout` is per-item — the kernel NEVER defaults it (absent = wait forever).
Expiry goes through the FRONT DOOR (`cancel_task`), so every bundle's
`on_cancel` hook runs before the abort.

Statuses: completed | failed | cancelled | timed_out.
Invariant: `pid is None` ⟺ the item never reached the task table.
"""

from __future__ import annotations

import asyncio
import sys
import time
import types

from kernel._kernel import Kernel

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
sys.modules["batch_slow_agent.tools"] = _mod

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
    from kernel._kernel import sender_context

    await _mk(seeded_kernel, "gated_a")  # deny-all: no can_call grants
    await _mk(seeded_kernel, "gated_b")  # sibling — outside the $parent baseline
    with sender_context("gated_a"):
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
    sys.modules["dawdle_agent.tools"] = mod
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
    sys.modules["raiser_agent.tools"] = mod
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
