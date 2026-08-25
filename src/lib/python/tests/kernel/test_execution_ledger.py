"""The EXECUTION LEDGER — caller-NAMED executions (C9 / C10 / C11).

`send([{target, payload, id: "job-42"}])` means ENSURE that execution exists —
the semantics a supplied `create_agent` id already has, applied to dispatch:

    unknown id   ⇒ dispatch and record          (a name, not a new mechanism)
    in flight    ⇒ JOIN — no second run         (C10: once-only under concurrency)
    settled      ⇒ the recorded outcome returns (C11: idempotency; C9 in-process)

ONE id space: the caller id IS the pid `ps`/`cancel`/reply tags carry. Minted
pids are `<agent>:<hex>`, so caller ids may not contain ":" and can never
collide. An id is BOUND to its first execution's target — the same id aimed
elsewhere refuses instead of returning an unrelated outcome.

Exemplars this transfers from: `dbos/tests/test_singleexec.py` (best in corpus),
`dbos/tests/test_duplication_policy.py`, re-keyed from "workflow step" to
"guarded call" per agentic_development.md §10.5 bucket 2.
"""

from __future__ import annotations

import asyncio
import sys
import types

_runs: list[str] = []
_gate = {}


def _flag(name: str) -> asyncio.Event:
    return _gate.setdefault(name, asyncio.Event())


_mod = types.ModuleType("ledger_probe.tools")


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "ledger probe", "verbs": {}}
    if verb == "work":
        _runs.append(id)
        await asyncio.sleep(payload.get("secs", 0.05))
        return {"run": len(_runs)}
    if verb == "slow":
        _flag(id).set()
        await asyncio.sleep(30)
        return {"finished": True}
    if verb == "boom":
        _runs.append(id)
        raise RuntimeError("kaput")
    return {"error": f"unknown verb {verb!r}"}


_mod.handler = _handler
_mod.VERBS = {}
sys.modules["ledger_probe.tools"] = _mod


async def _mk(kernel, agent_id="lp"):
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "ledger_probe.tools",
                        "id": agent_id,
                    },
                }
            ]
        )
    )[0]["result"]
    return rec["id"]


async def _rows(kernel):
    r = (await kernel.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0]
    return r["result"]["tasks"]


# ─── C10: once-only under concurrency ───────────────────────────


async def test_concurrent_same_id_runs_the_body_once(seeded_kernel):
    """Two items, one id, one batch: ONE execution, both outcomes identical,
    both carrying the caller's id as their pid."""
    _runs.clear()
    a = await _mk(seeded_kernel)
    o1, o2 = await seeded_kernel.send(
        [
            {"target": a, "payload": {"type": "work"}, "id": "once"},
            {"target": a, "payload": {"type": "work"}, "id": "once"},
        ]
    )
    assert len(_runs) == 1, f"the body ran {len(_runs)} times"
    assert o1 == o2
    assert o1["pid"] == "once" and o1["status"] == "completed"
    assert o1["result"] == {"run": 1}


async def test_two_racing_batches_share_one_execution(seeded_kernel):
    """The join works ACROSS batches, not only within one."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_race")
    b1 = asyncio.create_task(
        seeded_kernel.send(
            [{"target": a, "payload": {"type": "work", "secs": 0.2}, "id": "race"}]
        )
    )
    await asyncio.sleep(0.05)  # first batch owns the execution by now
    b2 = asyncio.create_task(
        seeded_kernel.send(
            [{"target": a, "payload": {"type": "work", "secs": 0.2}, "id": "race"}]
        )
    )
    (o1,), (o2,) = await asyncio.gather(b1, b2)
    assert len(_runs) == 1
    assert o1 == o2


# ─── C11 / C9: idempotent re-send ───────────────────────────────


async def test_a_settled_id_returns_the_recorded_outcome(seeded_kernel):
    """Re-sending a settled id runs NOTHING and returns what was recorded —
    a completed side effect is never re-fired (C9's in-process half)."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_idem")
    first = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "work"}, "id": "idem"}]
        )
    )[0]
    again = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "work"}, "id": "idem"}]
        )
    )[0]
    assert len(_runs) == 1, "the re-send re-executed the body"
    assert again == first


async def test_a_failed_outcome_is_recorded_too(seeded_kernel):
    """A raise settles as `failed` — and that IS the recorded outcome. A retry
    is a NEW id; the old id keeps telling the truth about what happened."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_boom")
    first = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "boom"}, "id": "boom1"}]
        )
    )[0]
    again = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "boom"}, "id": "boom1"}]
        )
    )[0]
    assert first["status"] == "failed" and first["result"]["raised"] is True
    assert again == first
    assert len(_runs) == 1


async def test_a_cancelled_execution_stays_cancelled(seeded_kernel):
    """`cancel` settles the id as cancelled; a re-send of the SAME id gets that
    outcome without running anything — a retry must choose a new id."""
    a = await _mk(seeded_kernel, "lp_cx")
    batch = asyncio.create_task(
        seeded_kernel.send([{"target": a, "payload": {"type": "slow"}, "id": "cx1"}])
    )
    await asyncio.wait_for(_flag(a).wait(), timeout=2)
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "cancel", "task_id": "cx1"}}]
        )
    )[0]["result"]
    assert r["cancelled"] is True, "the caller id IS the cancel handle"
    (o1,) = await asyncio.wait_for(batch, timeout=2)
    assert o1["status"] == "cancelled"
    again = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "slow"}, "id": "cx1"}]
        )
    )[0]
    assert again["status"] == "cancelled"
    assert all(t["id"] != "cx1" for t in await _rows(seeded_kernel))


# ─── the id contract ────────────────────────────────────────────


async def test_no_id_means_independent_executions(seeded_kernel):
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_free")
    o1, o2 = await seeded_kernel.send(
        [
            {"target": a, "payload": {"type": "work"}},
            {"target": a, "payload": {"type": "work"}},
        ]
    )
    assert len(_runs) == 2
    assert o1["pid"] != o2["pid"]


async def test_bad_ids_are_refused_before_dispatch(seeded_kernel):
    a = await _mk(seeded_kernel, "lp_bad")
    for bad in ("", 7, "has:colon"):
        out = (
            await seeded_kernel.send(
                [{"target": a, "payload": {"type": "work"}, "id": bad}]
            )
        )[0]
        assert out["status"] == "failed" and out["pid"] is None, bad
        assert out["result"]["reason"] == "bad_arg", bad


async def test_an_id_is_bound_to_its_first_target(seeded_kernel):
    """The same id aimed at a DIFFERENT agent refuses — returning an unrelated
    outcome would be silent corruption wearing idempotency's clothes."""
    a = await _mk(seeded_kernel, "lp_b1")
    b = await _mk(seeded_kernel, "lp_b2")
    await seeded_kernel.send([{"target": a, "payload": {"type": "work"}, "id": "bd"}])
    out = (
        await seeded_kernel.send(
            [{"target": b, "payload": {"type": "work"}, "id": "bd"}]
        )
    )[0]
    assert out["status"] == "failed"
    assert out["result"]["reason"] == "bad_arg"
    assert "lp_b1" in out["result"]["error"], "must NAME the bound target"


async def test_a_joiners_timeout_bounds_its_wait_not_the_execution(seeded_kernel):
    """A joiner does not own the execution: its `timeout` expiry settles the
    JOINER as timed_out while the execution keeps running for its owner."""
    a = await _mk(seeded_kernel, "lp_jt")
    owner = asyncio.create_task(
        seeded_kernel.send([{"target": a, "payload": {"type": "slow"}, "id": "jt1"}])
    )
    await asyncio.wait_for(_flag(a).wait(), timeout=2)
    joiner = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "slow"}, "id": "jt1", "timeout": 0.1}]
        )
    )[0]
    assert joiner["status"] == "timed_out"
    assert "keeps running" in joiner["result"]["error"]
    rows = await _rows(seeded_kernel)
    assert any(t["id"] == "jt1" for t in rows), "the execution must SURVIVE"
    # clean up: the owner's slow task
    await seeded_kernel.send(
        [{"target": "kernel", "payload": {"type": "cancel", "task_id": "jt1"}}]
    )
    await asyncio.wait_for(owner, timeout=2)


async def test_a_named_kernel_reflect_is_ledgered_not_ignored(seeded_kernel):
    """The identity-read fast path honors the id too — a field silently
    dropped on one path is exactly the drift this ledger exists to prevent."""
    o1 = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect"}, "id": "kr1"}]
        )
    )[0]
    o2 = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect"}, "id": "kr1"}]
        )
    )[0]
    assert o1["pid"] == "kr1" and o1["status"] == "completed"
    assert o2 == o1
