"""The EXECUTION LEDGER — caller-NAMED executions (C9 / C10 / C11).

`send([{target, payload, id: "job-42"}])` means ENSURE that execution exists —
the semantics a supplied `create_agent` id already has, applied to dispatch:

    unknown id   ⇒ dispatch and record          (a name, not a new mechanism)
    in flight    ⇒ JOIN — no second run         (C10: once-only under concurrency)
    settled      ⇒ the recorded outcome returns (C11: idempotency; C9 in-process)

ONE id space: a caller supplies only NAMES, and the kernel mints the numeric pids
`ps`/`cancel`/reply tags carry, so a name (any shape, a ":" included) can never
collide with a pid. An id is BOUND to its first execution's target — the same id aimed
elsewhere refuses instead of returning an unrelated outcome.

Exemplars this transfers from: `dbos/tests/test_singleexec.py` (best in corpus),
`dbos/tests/test_duplication_policy.py`, re-keyed from "workflow step" to
"guarded call" per agentic_development.md §10.5 bucket 2.
"""

from __future__ import annotations

import asyncio
import types
from kernel import register_unlisted

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
    if verb == "blob":
        # raw bytes BOTH ways — the tape's one reduction, in and out
        return {"echo": payload.get("bytes"), "note": "verbatim"}
    if verb == "slow":
        _flag(id).set()
        await asyncio.sleep(30)
        return {"finished": True}
    if verb == "boom":
        _runs.append(id)
        raise RuntimeError("kaput")
    if verb == "flaky":
        # fails until the Nth run — the retry engine's canonical target
        _runs.append(id)
        if len(_runs) < payload.get("succeed_on", 3):
            raise RuntimeError(f"flaky miss {len(_runs)}")
        return {"made_it": len(_runs)}
    return {"error": f"unknown verb {verb!r}"}


_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("ledger_probe.tools", _mod)


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
    assert o1["name"] == "once" and isinstance(o1["pid"], int)
    assert o1["status"] == "completed"
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
    for bad in ("", 7, [1], True):  # empty name · an int naming nothing · not an id
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
    assert any(t["id"] == joiner["pid"] for t in rows), "the execution must SURVIVE"
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
    assert o1["name"] == "kr1" and o1["status"] == "completed"
    assert o2 == o1


# ─── the retry engine (C12's machinery, caller-declared budget) ──


async def test_retries_rerun_a_failing_body_until_it_succeeds(seeded_kernel):
    """`retries: N` on a NAMED item: a terminal failed attempt re-dispatches
    under the SAME id until the budget is spent — the caller sees ONE outcome,
    the final one, with `attempts` telling the truth."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_flaky")
    out = (
        await seeded_kernel.send(
            [
                {
                    "target": a,
                    "payload": {"type": "flaky", "succeed_on": 3},
                    "id": "fl1",
                    "retries": 5,
                }
            ]
        )
    )[0]
    assert out["status"] == "completed", out
    assert out["result"] == {"made_it": 3}
    assert out["attempts"] == 3
    assert len(_runs) == 3


async def test_a_spent_budget_settles_as_the_last_failure(seeded_kernel):
    """Budget spent ⇒ the row parks with its final failure — THE dead letter.
    Nothing retries it again: a re-send of the id returns the recorded outcome."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_dl")
    out = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "boom"}, "id": "dl1", "retries": 1}]
        )
    )[0]
    assert out["status"] == "failed" and out["attempts"] == 2
    assert len(_runs) == 2
    again = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "boom"}, "id": "dl1", "retries": 9}]
        )
    )[0]
    assert again["status"] == "failed" and len(_runs) == 2, (
        "a parked dead letter must never fire again"
    )


async def test_no_retries_means_exactly_todays_behavior(seeded_kernel):
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_r0")
    out = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "boom"}, "id": "r0"}]
        )
    )[0]
    assert out["status"] == "failed" and out["attempts"] == 1
    assert len(_runs) == 1


async def test_completed_never_retries(seeded_kernel):
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_c1")
    out = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "work"}, "id": "c1", "retries": 3}]
        )
    )[0]
    assert out["status"] == "completed" and out["attempts"] == 1
    assert len(_runs) == 1


async def test_cancel_is_absorbing_and_stops_the_retry_loop(seeded_kernel):
    """A cancel is a HUMAN decision, not a transient — the loop must not spend
    its budget resurrecting what an operator just killed."""
    a = await _mk(seeded_kernel, "lp_cxr")
    batch = asyncio.create_task(
        seeded_kernel.send(
            [{"target": a, "payload": {"type": "slow"}, "id": "cxr", "retries": 5}]
        )
    )
    await asyncio.wait_for(_flag(a).wait(), timeout=2)
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "cancel", "task_id": "cxr"}}]
        )
    )[0]["result"]
    assert r["cancelled"] is True
    (out,) = await asyncio.wait_for(batch, timeout=2)
    assert out["status"] == "cancelled" and out["attempts"] == 1
    assert all(t["id"] != "cxr" for t in await _rows(seeded_kernel))


async def test_retries_ride_unnamed_items_too(seeded_kernel):
    """Ledger-all: every execution has a row, so `retries` needs no name — the
    minted pid is the row key. Bad values still refuse."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_noid")
    out = (
        await seeded_kernel.send(
            [
                {
                    "target": a,
                    "payload": {"type": "flaky", "succeed_on": 2},
                    "retries": 3,
                }
            ]
        )
    )[0]
    assert out["status"] == "completed" and out["attempts"] == 2
    assert len(_runs) == 2
    for bad in (-1, True, "3"):
        out = (
            await seeded_kernel.send(
                [
                    {
                        "target": a,
                        "payload": {"type": "work"},
                        "id": f"b{bad}",
                        "retries": bad,
                    }
                ]
            )
        )[0]
        assert out["status"] == "failed" and out["result"]["reason"] == "bad_arg"


# ─── the `ledger` verb: the read surface / dead-letter queue ─────


async def test_ledger_lists_named_executions_with_outcomes(seeded_kernel):
    """`ps` is work in flight; `ledger` is what named work BECAME. Terminal
    non-completed rows are the dead-letter queue — inspectable, outcome
    attached, never silently retried."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_lv")
    outs = await seeded_kernel.send(
        [
            {"target": a, "payload": {"type": "work"}, "id": "lv_ok"},
            {"target": a, "payload": {"type": "boom"}, "id": "lv_dead", "retries": 1},
            {"target": a, "payload": {"type": "work"}},  # unnamed: listed under pid
        ]
    )
    led = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "ledger"}}])
    )[0]["result"]
    rows = {r["id"]: r for r in led["executions"]}
    rows.update({r["name"]: r for r in led["executions"] if r.get("name")})
    minted = outs[2]["pid"]
    assert {"lv_ok", "lv_dead", minted} <= set(rows), "LEDGER-ALL: unnamed too"
    assert rows[minted]["named"] is False and rows["lv_ok"]["named"] is True
    assert rows["lv_ok"]["status"] == "completed"
    dead = rows["lv_dead"]
    assert dead["status"] == "failed" and dead["attempts"] == 2
    assert dead["retries"] == 1
    assert dead["outcome"]["result"]["raised"] is True, "the WHY travels with it"


async def test_ledger_shows_in_flight_and_pages(seeded_kernel):
    a = await _mk(seeded_kernel, "lp_lp")
    batch = asyncio.create_task(
        seeded_kernel.send([{"target": a, "payload": {"type": "slow"}, "id": "lp_run"}])
    )
    await asyncio.wait_for(_flag(a).wait(), timeout=2)
    await seeded_kernel.send(
        [{"target": a, "payload": {"type": "work"}, "id": "lp_done"}]
    )
    led = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "ledger", "limit": 1}}]
        )
    )[0]["result"]
    assert len(led["executions"]) == 1 and led["next"], "paged like every listing"
    rows = {}
    payload = {"type": "ledger"}
    while True:
        led = (await seeded_kernel.send([{"target": "kernel", "payload": payload}]))[0][
            "result"
        ]
        rows.update({r["id"]: r for r in led["executions"]})
        rows.update({r["name"]: r for r in led["executions"] if r.get("name")})
        if not led["next"]:
            break
        payload = {"type": "ledger", "after": led["next"]}
    assert rows["lp_run"]["status"] == "in_flight"
    assert "outcome" not in rows["lp_run"]
    assert rows["lp_done"]["status"] == "completed"
    await seeded_kernel.send(
        [{"target": "kernel", "payload": {"type": "cancel", "task_id": "lp_run"}}]
    )
    await asyncio.wait_for(batch, timeout=2)


# Rust twin: fantastic-kernel src/send/ledger_tests.rs::a_send_issued_from_a_spawned_fork_is_in_the_operators_ledger
async def test_a_send_issued_from_a_spawned_fork_is_in_the_operators_ledger(
    seeded_kernel,
):
    from _testkit import as_agent, wire

    b = await _mk(seeded_kernel, "l_peer")
    a = await _mk(seeded_kernel, "l_forker")
    wire(seeded_kernel, a, b)  # a refused send never reaches the ledger at all

    async def fork():
        await seeded_kernel.send(
            [{"target": b, "payload": {"type": "work", "secs": 0.0}}]
        )

    with as_agent(seeded_kernel, a):
        tid = seeded_kernel.ctx.spawn_task(a, "route_reply", fork())
    for _ in range(100):
        if tid not in seeded_kernel.ctx.tasks:
            break
        await asyncio.sleep(0.01)

    led = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "ledger"}}])
    )[0]["result"]
    rows = led["executions"]
    assert any(
        r["target"] == b
        and r["status"] == "completed"
        and isinstance((r.get("outcome") or {}).get("bytes"), (int, float))
        for r in rows
    ), f"a forked agent's send left no ledger row: {rows!r}"


# ─── ledger-ALL: one id space, one engine ────────────────────────


async def test_a_minted_pid_is_referencable_for_refetch(seeded_kernel):
    """A colon id NAMES an existing minted execution — re-fetch its recorded
    outcome; it never creates one (the kernel is the only allocator there)."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_ref")
    first = (await seeded_kernel.send([{"target": a, "payload": {"type": "work"}}]))[0]
    pid = first["pid"]
    assert isinstance(pid, int)
    again = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "work"}, "id": pid}]
        )
    )[0]
    assert len(_runs) == 1, "the re-fetch must not re-run"
    assert again["result"] == first["result"]
    ghost = (
        await seeded_kernel.send(
            [
                {"target": a, "payload": {"type": "work"}, "id": 999999999}
            ]  # minted-shaped, unknown
        )
    )[0]
    assert ghost["status"] == "failed"
    assert ghost["result"]["reason"] == "bad_arg"
    assert len(_runs) == 1


async def test_the_ring_evicts_oldest_settled_unnamed_first(seeded_kernel):
    """`ledger_cap` bounds history: oldest SETTLED unnamed rows go first; named
    rows (a dedup contract) survive them; in-flight rows are never evicted."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_ring")
    seeded_kernel.ctx.ledger_cap = 3
    try:
        named = (
            await seeded_kernel.send(
                [{"target": a, "payload": {"type": "work"}, "id": "keepme"}]
            )
        )[0]
        pids = []
        for _ in range(4):
            o = (
                await seeded_kernel.send([{"target": a, "payload": {"type": "work"}}])
            )[0]
            pids.append(o["pid"])
        led = seeded_kernel.ctx.executions
        assert "keepme" in seeded_kernel.ctx.names, (
            "a named row must outlive unnamed churn"
        )
        assert len(led) == 3
        assert pids[0] not in led and pids[1] not in led, "oldest evicted"
        assert pids[-1] in led
        # named dedup still holds after the churn
        again = (
            await seeded_kernel.send(
                [{"target": a, "payload": {"type": "work"}, "id": "keepme"}]
            )
        )[0]
        assert again == named or again["result"] == named["result"]
    finally:
        seeded_kernel.ctx.ledger_cap = 1_000_000


async def test_ledger_view_is_origin_scoped(seeded_kernel):
    """The ledger carries RESULT BODIES — an unscoped read is a side-channel
    around `callable_by`. An agent sees only what IT originated; the operator
    sees everything."""
    from _testkit import as_agent

    a = await _mk(seeded_kernel, "lp_scope")
    b = await _mk(seeded_kernel, "lp_spy")
    # grant the spy the verb itself + a target of its own
    await seeded_kernel.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "update_agent",
                    "id": b,
                    "can_call": {"$self": "*", "kernel_state": ["ledger"], a: "*"},
                },
            },
            {
                "target": "kernel",
                "payload": {
                    "type": "update_agent",
                    "id": a,
                    "callable_by": {"$self": "*", b: "*"},
                },
            },
            {
                "target": "kernel",
                "payload": {
                    "type": "update_agent",
                    "id": "kernel_state",
                    "callable_by": {"$self": "*", b: ["ledger"]},
                },
            },
        ]
    )
    # operator-run work — the secret the spy must not see
    await seeded_kernel.send(
        [{"target": a, "payload": {"type": "work"}, "id": "op_secret"}]
    )
    # spy-run work — what the spy MAY see
    with as_agent(seeded_kernel, b):
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "work"}, "id": "spy_own"}]
        )
        led = (
            await seeded_kernel.send(
                [{"target": "kernel", "payload": {"type": "ledger"}}]
            )
        )[0]["result"]
    seen = {r.get("name") or r["id"] for r in led["executions"]}
    assert "spy_own" in seen
    assert "op_secret" not in seen, "another origin's results leaked"
    op = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "ledger"}}])
    )[0]["result"]
    assert {"op_secret", "spy_own"} <= {r.get("name") for r in op["executions"]}


# ─── C9 ACROSS RESTART: the durable half ────────────────────────
#
# The kernel's execution rows live in its MEMORY STORE (`memory_store_id` on the
# root); the flusher writes them there and drops them from RAM. A restart is a new
# kernel on the same dir (same guid, same id bound) whose store holds what the old
# one flushed — a ram_store dies with its process, so the test carries its tables
# across with `dump` → `inject`, exactly what a durable store does by itself.


async def _boot_with_store(tmp_path, monkeypatch):
    from _testkit import boot_root

    monkeypatch.chdir(tmp_path)
    k = boot_root()
    await k.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": k.id,
                    "handler_module": "ram_store.tools",
                    "id": "memory",
                },
            }
        ]
    )
    k.ctx.update(k.id, memory_store_id="memory")
    out = await k.ctx.start(boot_all=False)
    assert out.get("started"), out
    return k


async def _dump(k) -> dict:
    return (await k.send([{"target": "memory", "payload": {"type": "dump"}}]))[0][
        "result"
    ]["tables"]


async def _reboot(k1):
    """Flush everything, persist the records, come back up on the same store."""
    from _testkit import boot_root, persist

    await k1.ctx.flush_executions()
    tables = await _dump(k1)
    persist(k1)
    del k1
    k2 = boot_root()
    await k2.send(
        [{"target": "memory", "payload": {"type": "inject", "tables": tables}}]
    )
    out = await k2.ctx.start(boot_all=False)
    assert out.get("started"), out
    return k2


def _kernel_rows(k, tables: dict) -> list[dict]:
    guid = k.record["guid"]
    return [r["row"] for r in tables["rows"] if r["owner"] == f"{guid}|kernel"]


async def test_a_completed_execution_survives_reboot(tmp_path, monkeypatch):
    """C9, the full property: a completed step is NEVER re-executed — not even
    by a fresh process. The settled call comes back from its `call` + `settle`
    rows in the kernel's store; a re-send of the id returns it and the handler
    does not run."""
    _runs.clear()
    k1 = await _boot_with_store(tmp_path, monkeypatch)
    a = await _mk(k1, "lp_reboot")
    first = (
        await k1.send([{"target": a, "payload": {"type": "work"}, "id": "boot-1"}])
    )[0]
    assert first["status"] == "completed" and len(_runs) == 1

    k2 = await _reboot(k1)
    again = (
        await k2.send([{"target": a, "payload": {"type": "work"}, "id": "boot-1"}])
    )[0]
    assert len(_runs) == 1, "REBOOT re-executed a completed step — C9 broken"
    assert again["result"] == first["result"]
    assert again["attempts"] == 1


async def test_the_store_carries_the_payload_and_the_full_outcome(
    tmp_path, monkeypatch
):
    """The kernel's rows in its store hold the call WITH its dispatch payload and
    the settle WITH the whole result body — nothing RAM-only."""
    _runs.clear()
    k1 = await _boot_with_store(tmp_path, monkeypatch)
    a = await _mk(k1, "lp_tape")
    out = (
        await k1.send(
            [{"target": a, "payload": {"type": "work", "secs": 0}, "id": "tape-1"}]
        )
    )[0]
    k2 = await _reboot(k1)

    rows = _kernel_rows(k2, await _dump(k2))
    call = next(r for r in rows if r.get("name") == "tape-1")
    assert call["payload"] == {"type": "work", "secs": 0}
    assert call["named"] is True and call["retries"] == 0 and call["attempts"] == 1
    settle = next(
        r for r in rows if r.get("kind") == "settle" and r.get("parent") == call["id"]
    )
    assert settle["outcome"] == {"status": "completed", "result": out["result"]}
    assert settle["attempts"] == 1
    # and the `ledger` verb answers the FULL outcome after the restart
    led = (await k2.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
        "result"
    ]
    row = next(r for r in led["executions"] if r.get("name") == "tape-1")
    assert row["outcome"]["result"] == out["result"]


async def test_raw_bytes_in_the_store_are_a_length_not_base64(tmp_path, monkeypatch):
    """The ONE reduction: a `bytes` value anywhere in a payload or a result is
    stored as its LENGTH. Base64 is banned; text and JSON stay verbatim."""
    k1 = await _boot_with_store(tmp_path, monkeypatch)
    a = await _mk(k1, "lp_blob")
    await k1.send(
        [
            {
                "target": a,
                "payload": {
                    "type": "blob",
                    "path": "probe.bin",
                    "bytes": b"\x00\x01\x02\x03\x04",
                },
                "id": "bin-1",
            }
        ]
    )
    await k1.ctx.flush_executions()
    rows = _kernel_rows(k1, await _dump(k1))
    call = next(r for r in rows if r.get("name") == "bin-1")
    assert call["payload"]["bytes"] == {"len": 5}
    assert call["payload"]["path"] == "probe.bin"  # everything else verbatim
    settle = next(
        r for r in rows if r.get("kind") == "settle" and r.get("parent") == call["id"]
    )
    assert settle["outcome"]["result"] == {"echo": {"len": 5}, "note": "verbatim"}


async def test_a_dead_letter_survives_reboot_and_stays_dead(tmp_path, monkeypatch):
    _runs.clear()
    k1 = await _boot_with_store(tmp_path, monkeypatch)
    a = await _mk(k1, "lp_rdead")
    dead = (
        await k1.send(
            [{"target": a, "payload": {"type": "boom"}, "id": "dead-1", "retries": 1}]
        )
    )[0]
    assert dead["status"] == "failed" and dead["attempts"] == 2

    k2 = await _reboot(k1)
    led = (await k2.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
        "result"
    ]
    rows = {r["id"]: r for r in led["executions"]}
    rows.update({r["name"]: r for r in led["executions"] if r.get("name")})
    assert rows["dead-1"]["status"] == "failed" and rows["dead-1"]["attempts"] == 2
    again = (
        await k2.send(
            [{"target": a, "payload": {"type": "boom"}, "id": "dead-1", "retries": 5}]
        )
    )[0]
    assert again["status"] == "failed" and len(_runs) == 2, (
        "a parked dead letter fired again after reboot"
    )


async def test_in_flight_work_does_not_survive_a_crash(tmp_path, monkeypatch):
    """Only SETTLED calls come back as executions. Work in flight at the crash
    never completed; a re-send after the restart RE-EXECUTING it is the truth, not a
    loss. The call row is in the store either way — with no settle after it."""
    import asyncio as _a

    from _testkit import boot_root, persist

    _runs.clear()
    k1 = await _boot_with_store(tmp_path, monkeypatch)
    a = await _mk(k1, "lp_crash")
    batch = _a.create_task(
        k1.send([{"target": a, "payload": {"type": "slow"}, "id": "mid-1"}])
    )
    await _a.wait_for(_flag(a).wait(), timeout=2)
    await k1.ctx.flush_executions()  # the "crash" happens with mid-1 unsettled
    tables = await _dump(k1)
    persist(k1)
    batch.cancel()
    try:
        await batch
    except _a.CancelledError:
        pass

    del k1
    k2 = boot_root()
    await k2.send(
        [{"target": "memory", "payload": {"type": "inject", "tables": tables}}]
    )
    await k2.ctx.start(boot_all=False)
    led = (await k2.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
        "result"
    ]
    mid = [r for r in led["executions"] if r.get("name") == "mid-1"]
    assert mid and mid[0]["status"] == "in_flight", "its call row, no ending"
    again = (
        await k2.send([{"target": a, "payload": {"type": "work"}, "id": "mid-1"}])
    )[0]
    assert again["status"] == "completed", "an unsettled step re-runs"


async def test_an_unstarted_kernel_keeps_its_rows_in_ram(tmp_path, monkeypatch):
    """No `kernel_start`, no store: the rows stay in RAM and die with the process;
    nothing errors and nothing is written."""
    from _testkit import boot_root

    monkeypatch.chdir(tmp_path)
    k1 = boot_root()
    await k1.send([{"target": k1.id, "payload": {"type": "boot"}}])
    a = await _mk(k1, "lp_ram")
    out = (await k1.send([{"target": a, "payload": {"type": "work"}, "id": "ram-1"}]))[
        0
    ]
    assert out["status"] == "completed"
    assert out["pid"] in k1.ctx.rows
    assert (await k1.ctx.flush_executions()) == {"flushed": 0}


# ─── uncovered branches of the EXISTING engine (no new code) ─────


async def test_an_in_flight_minted_pid_can_be_joined(seeded_kernel):
    """The colon reference works on LIVE executions too: a second caller who
    learned a minted pid joins the one run instead of racing it."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_livejoin")
    b1 = asyncio.create_task(
        seeded_kernel.send([{"target": a, "payload": {"type": "work", "secs": 0.3}}])
    )
    await asyncio.sleep(0.05)
    pid = next(t["id"] for t in await _rows(seeded_kernel) if t["agent"] == a)
    assert isinstance(pid, int)
    joined = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "work"}, "id": pid}]
        )
    )[0]
    (owner,) = await b1
    assert len(_runs) == 1, "the join must not have raced a second run"
    assert joined["result"] == owner["result"] and joined["pid"] == pid


async def test_timed_out_attempts_also_spend_the_retry_budget(seeded_kernel):
    """`retries` covers timeouts, not only raises: each attempt gets a fresh
    clock, each expiry goes through the front door, and the final outcome is
    `timed_out` with the true attempt count."""
    a = await _mk(seeded_kernel, "lp_tor")
    out = (
        await seeded_kernel.send(
            [
                {
                    "target": a,
                    "payload": {"type": "slow"},
                    "id": "tor1",
                    "timeout": 0.15,
                    "retries": 2,
                }
            ]
        )
    )[0]
    assert out["status"] == "timed_out"
    assert out["attempts"] == 3
    assert out["result"]["reason"] == "timeout"
    assert all(t["id"] != "tor1" for t in await _rows(seeded_kernel))


async def test_cancel_reaches_the_joiner_too(seeded_kernel):
    """One execution, two waiters: `cancel <id>` settles BOTH with the same
    cancelled outcome — a joiner is never left hanging on a killed run."""
    a = await _mk(seeded_kernel, "lp_cj")
    owner = asyncio.create_task(
        seeded_kernel.send([{"target": a, "payload": {"type": "slow"}, "id": "cj1"}])
    )
    await asyncio.wait_for(_flag(a).wait(), timeout=2)
    joiner = asyncio.create_task(
        seeded_kernel.send([{"target": a, "payload": {"type": "slow"}, "id": "cj1"}])
    )
    await asyncio.sleep(0.05)
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "cancel", "task_id": "cj1"}}]
        )
    )[0]["result"]
    assert r["cancelled"] is True
    (o1,), (o2,) = await asyncio.gather(owner, joiner)
    assert o1["status"] == "cancelled" and o2["status"] == "cancelled"
    assert o1["result"] == {"cancelled": True} and o2["result"] == o1["result"]


async def test_a_joiner_during_the_retry_loop_sees_only_the_final_outcome(
    seeded_kernel,
):
    """Intermediate attempts are invisible to everyone: a caller joining while
    attempt 1 is failing receives the eventual success, attempts included."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_jr")
    owner = asyncio.create_task(
        seeded_kernel.send(
            [
                {
                    "target": a,
                    "payload": {"type": "flaky", "succeed_on": 3, "secs": 0.2},
                    "id": "jr1",
                    "retries": 5,
                }
            ]
        )
    )
    await asyncio.sleep(0.05)  # inside an early (failing) attempt
    joiner = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "flaky"}, "id": "jr1"}]
        )
    )[0]
    (o1,) = await owner
    assert o1["status"] == "completed" and o1["attempts"] == 3
    assert joiner == o1, "the joiner saw an intermediate attempt"


# ─── joining is gated on the execution it joins ─────────────────


async def test_joining_is_authorized_against_the_verb_that_execution_ran(
    seeded_kernel,
):
    """A join must ask the gate the RIGHT QUESTION.

    Checking the joiner's own payload verb, then handing back the ORIGINAL call's
    outcome, would return a reply for a verb the joiner never had authorized: an
    agent granted only `safe_verb` on a target could reuse an id another agent used
    for `create_sensitive` and receive its secret verbatim. The handler runs once
    (dedup is correct); the RESULT would cross an authorization boundary.

    Same class as the read side-channel closed by origin-scoping the `ledger` verb —
    the same leak through `send` itself, reachable from the wire, where a frame id IS
    the execution id.

    Joining is equivalent to having made that call, so that is what is asked.
    """
    from _testkit import as_agent

    k = seeded_kernel
    await _mk(k, "vault")
    k.ctx.agents["vault"]._meta["callable_by"] = {
        "alice": ["sensitive"],
        "bob": ["harmless"],
    }
    for who, grant in (("alice", ["sensitive"]), ("bob", ["harmless"])):
        await _mk(k, who)
        k.ctx.agents[who]._meta["can_call"] = {"vault": grant}

    with as_agent(seeded_kernel, "alice"):
        a = (
            await k.send(
                [{"target": "vault", "id": "job1", "payload": {"type": "sensitive"}}]
            )
        )[0]
    # `errored`: the vault HANDLER ran and answered "unknown verb". A semantic
    # no, not a transport failure — and a settled outcome either way, which is
    # what makes the join below the thing under test.
    assert a["status"] == "errored", a

    with as_agent(seeded_kernel, "bob"):
        b = (
            await k.send(
                [{"target": "vault", "id": "job1", "payload": {"type": "harmless"}}]
            )
        )[0]
    assert b["status"] == "failed", f"bob joined an execution it may not run: {b}"
    assert b["result"].get("reason") == "unauthorized", b["result"]

    # An AUTHORIZED re-sender still dedups — the fix must not cost ensure-semantics.
    with as_agent(seeded_kernel, "alice"):
        again = (
            await k.send(
                [{"target": "vault", "id": "job1", "payload": {"type": "sensitive"}}]
            )
        )[0]
    assert again["result"] == a["result"], "the recorded outcome stopped being returned"


# ─── twins of the Rust ledger unit tests (#170) ──────────────────


async def test_a_name_is_just_a_name_and_the_kernel_owns_every_id(seeded_kernel):
    """THERE IS NO PID NAMESPACE: a caller supplies only a NAME, the kernel supplies
    every id, and a name that looks like a pid is just an unusual name."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_pidlike")
    item = {"target": a, "payload": {"type": "work"}, "id": "desk:0badf00d"}
    first = (await seeded_kernel.send([dict(item)]))[0]
    # It ran as an ordinary name: not refused, and the kernel minted its id.
    assert first["status"] == "completed", first
    assert isinstance(first["pid"], int) and not isinstance(first["pid"], bool), first
    assert seeded_kernel.ctx.resolve_name("desk:0badf00d") == first["pid"]
    # And re-sending it JOINS, like any other name: the body does not run again.
    again = (await seeded_kernel.send([dict(item)]))[0]
    assert again == first
    assert len(_runs) == 1, "the re-send re-executed the body"


async def test_a_live_row_and_an_unknown_status_are_left_alone(
    seeded_kernel, monkeypatch
):
    """`recall_executions` puts back a flushed, SETTLED execution, but a LIVE entry
    always wins over the store, an unknown status is not guessed at, and a call row
    with no target is not an execution."""
    _runs.clear()
    a = await _mk(seeded_kernel, "lp_recall")
    k = seeded_kernel.ctx
    live = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "work"}, "id": "live-1"}]
        )
    )[0]
    live_id = live["pid"]
    held = k.executions[live_id]

    def call_row(eid, agent, name=None):
        row = {"id": eid, "kind": "call", "payload": {"type": "work"}, "retries": 0}
        if agent is not None:
            row["agent"] = agent
        if name:
            row.update(name=name, named=True)
        return row

    def settle(eid, status):
        return {
            "id": eid + 1000,
            "kind": "settle",
            "parent": eid,
            "attempts": 1,
            "outcome": {"status": status, "result": {"from": "store"}},
        }

    stored = {
        # the store disagrees about a LIVE execution (by name and by id)
        "live-1": (
            [call_row(live_id, "elsewhere", "live-1")],
            [settle(live_id, "failed")],
        ),
        # an unknown status
        "odd-1": ([call_row(900, a, "odd-1")], [settle(900, "in_flight")]),
        # a call row with no target
        "orphan-1": ([call_row(901, None, "orphan-1")], [settle(901, "completed")]),
    }

    async def fake_read(payload):
        if payload["kinds"] == ["call"]:
            m = payload.get("match") or {}
            ids = payload.get("ids") or []
            return [
                c
                for calls, _ in stored.values()
                for c in calls
                if c.get("name") == m.get("name") or c["id"] in ids
            ]
        parent = payload["match"]["parent"]
        return [s for _, sets in stored.values() for s in sets if s["parent"] == parent]

    monkeypatch.setattr(k, "_store_read", fake_read)
    await k.recall_executions(
        [{"id": "live-1"}, {"id": live_id}, {"id": "odd-1"}, {"id": "orphan-1"}]
    )

    # the live execution is the same object, still the real outcome
    assert k.executions[live_id] is held
    assert held["fut"].result()["status"] == "completed"
    assert held["target"] == a
    assert k.resolve_name("live-1") == live_id
    # neither of the other two came back
    assert 900 not in k.executions and 901 not in k.executions
    assert k.resolve_name("odd-1") is None and k.resolve_name("orphan-1") is None


async def test_forget_drops_the_entry_and_its_name(seeded_kernel):
    """Forgetting a settled named execution removes it from `executions` AND frees
    its name binding."""
    a = await _mk(seeded_kernel, "lp_forget")
    k = seeded_kernel.ctx
    out = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "work"}, "id": "gone-1"}]
        )
    )[0]
    eid = out["pid"]
    assert k.executions[eid]["fut"].done()
    assert k.resolve_name("gone-1") == eid
    k._forget_execution(eid)
    assert eid not in k.executions
    assert "gone-1" not in k.names
    assert k.resolve_name("gone-1") is None, "the name left with it"


# ─── Rust twins (#170): each `test_<rust test name>` asserts what the Rust test does ─

_twin_calls: list[str] = []


async def _twin_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "ledger twin probe", "verbs": {}}
    if verb == "work":
        _twin_calls.append(id)
        return {"done": "ok"}
    if verb == "echo_error":
        return {"error": payload.get("error")}
    if verb == "fork":

        async def body():
            kernel.ctx.rows_put(id, {"kind": "note"})
            _twin_calls.append("body")

        return {"task": kernel.ctx.spawn_task(id, "body", body())}
    if verb == "nope":
        return {"error": "н" * 300, "reason": "not_found"}
    return {"error": f"unknown verb {verb!r}"}


_twin_mod = types.ModuleType("ledger_twin_probe.tools")
_twin_mod.handler = _twin_handler
_twin_mod.VERBS = {}
register_unlisted("ledger_twin_probe.tools", _twin_mod)


async def _twin_agent(kernel, agent_id: str) -> str:
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "ledger_twin_probe.tools",
                        "id": agent_id,
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == agent_id, rec
    return agent_id


async def _ledger(kernel) -> list[dict]:
    r = (await kernel.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    return r["result"]["executions"]


def _rows_about(kernel, agent_id: str) -> list[dict]:
    return [r for r in kernel.ctx.rows.values() if r.get("agent") == agent_id]


async def test_ledger_row_key_order_matches_python(seeded_kernel):
    """A ledger row is exactly `[id, target, attempts, retries, named, name?, status,
    outcome]` -- `name` OMITTED (not null) when the execution is unnamed."""
    a = await _twin_agent(seeded_kernel, "l_key_order")
    await seeded_kernel.send([{"target": a, "payload": {"type": "work"}}])
    unnamed = next(
        r
        for r in await _ledger(seeded_kernel)
        if r["target"] == "l_key_order" and r["status"] == "completed"
    )
    assert list(unnamed) == [
        "id",
        "target",
        "attempts",
        "retries",
        "named",
        "status",
        "outcome",
    ], unnamed
    assert "name" not in unnamed

    await seeded_kernel.send(
        [{"target": a, "payload": {"type": "work"}, "id": "named-1"}]
    )
    named = next(r for r in await _ledger(seeded_kernel) if r.get("name") == "named-1")
    assert list(named) == [
        "id",
        "target",
        "attempts",
        "retries",
        "named",
        "name",
        "status",
        "outcome",
    ], named


async def test_an_errored_execution_is_visible_in_the_ledger(seeded_kernel):
    """A handler that RAN and said no is a ledger row like any other: the `ledger`
    verb lists that pid with its target and status `errored`."""
    a = await _twin_agent(seeded_kernel, "l_err")
    out = (
        await seeded_kernel.send([{"target": a, "payload": {"type": "nope_unknown"}}])
    )[0]
    assert out["status"] == "errored", out
    pid = out["pid"]
    hit = next((r for r in await _ledger(seeded_kernel) if r["id"] == pid), None)
    assert hit is not None, "the errored execution vanished from the ledger"
    assert hit["target"] == a
    assert hit["status"] == "errored"


async def test_a_dispatch_writes_a_call_row_with_outcome(seeded_kernel):
    a = await _twin_agent(seeded_kernel, "w")
    out = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "work"}, "id": "named-one"}]
        )
    )[0]
    rows = _rows_about(seeded_kernel, a)
    calls = [r for r in rows if r.get("kind") == "call" and r.get("verb") == "work"]
    assert len(calls) == 1, f"one call row on the TARGET: {rows}"
    call = calls[0]
    # PYTHON'S KEY ORDER: the row as written, `agent` + `ts` added by the store,
    # `outcome` stamped onto the end at settle.
    assert list(call) == [
        "id",
        "kind",
        "verb",
        "parent",
        "caller",
        "payload",
        "named",
        "retries",
        "attempts",
        "timeout",
        "name",
        "agent",
        "ts",
        "outcome",
    ], list(call)
    assert call["payload"] == {"type": "work"}
    assert call["named"] is True
    assert call["retries"] == 0
    assert call["attempts"] == 1
    assert call["timeout"] is None
    assert call["kind"] == "call"
    assert call["verb"] == "work"
    assert call["parent"] is None, "no dispatch was nested around it"
    assert call["caller"] is None, "an operator send has no origin"
    assert call["name"] == "named-one"
    assert call["id"] == out["pid"]
    assert call["outcome"] == {"status": "completed"}


async def test_an_errored_call_row_stamps_the_python_error_and_reason(seeded_kernel):
    a = await _twin_agent(seeded_kernel, "w_err")
    await seeded_kernel.send([{"target": a, "payload": {"type": "nope"}}])
    call = next(
        r
        for r in _rows_about(seeded_kernel, a)
        if r.get("kind") == "call" and r.get("verb") == "nope"
    )
    assert call["outcome"] == {
        "status": "errored",
        "reason": "not_found",
        "error": "н" * 200,
    }
    assert len(call["outcome"]) == 3, "nothing else is copied"


async def test_a_falsy_non_string_error_settles_completed(seeded_kernel):
    """FULL truthiness, not "the key is present": a falsy `error` settles completed,
    a truthy non-string one errored."""
    a = await _twin_agent(seeded_kernel, "w_falsy")
    for falsy in (0, 0.0, [], {}, "", False, None):
        out = (
            await seeded_kernel.send(
                [{"target": a, "payload": {"type": "echo_error", "error": falsy}}]
            )
        )[0]
        assert out["status"] == "completed", f"falsy error {falsy!r}: {out}"
    for truthy in (1, -1, [1], {"k": 1}):
        out = (
            await seeded_kernel.send(
                [{"target": a, "payload": {"type": "echo_error", "error": truthy}}]
            )
        )[0]
        assert out["status"] == "errored", f"truthy error {truthy!r}: {out}"


async def test_a_refusal_that_opened_no_call_row_appends_no_settle(seeded_kernel):
    """`pid is None` <=> nothing reached the task table: no ending is written for a
    beginning that was never written down."""
    await _twin_agent(seeded_kernel, "w_ghost")
    before = {i for i, r in seeded_kernel.ctx.rows.items() if r.get("kind") == "settle"}
    out = (await seeded_kernel.send([{"target": "ghost", "payload": {"type": "work"}}]))[0]
    assert out["pid"] is None, out
    after = {i for i, r in seeded_kernel.ctx.rows.items() if r.get("kind") == "settle"}
    assert after == before, "a refused send appended a settle row"


async def test_a_fork_is_rowed_before_the_dispatch_settles_and_its_body_runs_under_it(
    seeded_kernel,
):
    """`spawn_task` mints the fork's `task` row synchronously inside the handler, so
    it precedes the dispatch's `settle` row; the forked body's rows follow the task
    row that names it."""
    _twin_calls.clear()
    a = await _twin_agent(seeded_kernel, "f")
    out = (await seeded_kernel.send([{"target": a, "payload": {"type": "fork"}}]))[0]
    for _ in range(200):
        if "body" in _twin_calls:
            break
        await asyncio.sleep(0.01)
    assert "body" in _twin_calls, "the forked body never ran"
    rows = seeded_kernel.ctx.rows
    task = next(i for i, r in rows.items() if r.get("kind") == "task" and r["agent"] == a)
    settle = next(
        i for i, r in rows.items() if r.get("kind") == "settle" and r["parent"] == out["pid"]
    )
    note = next(i for i, r in rows.items() if r.get("kind") == "note" and r["agent"] == a)
    assert task < settle, "the fork's task row precedes the dispatch's settle row"
    assert task < note, "the body's rows follow the task row that names it"
