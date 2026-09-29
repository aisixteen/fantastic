"""The kernel keeps NO conversation and NO durable tape. Its execution rows (`call`,
`settle`, `task`, `ack`) live in RAM until the in-kernel FLUSHER writes them to the
root's MEMORY STORE (`memory_store_id`) and drops them; `ledger`, dead letters, `ack`
and named-call dedup read the store back. `kernel_start` refuses to run without a
store. Also here: the tail (`rows_stream`), the drain and `shutdown_agents`, the id
counter's block reservation, and the graceful stop landing its last rows."""

from __future__ import annotations

import asyncio
import sys
import types
from pathlib import Path

import yaml

from _testkit import boot_root, persist
from kernel import sender_context
from kernel._kernel import shutdown_context

# ── a probe bundle: advertises `shutdown`, can hold, fail, and count ───────────
_shut_calls: list[str] = []
_run_flag: dict[str, asyncio.Event] = {}
_counts: dict[str, int] = {}


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb == "boot":
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "probe", "verbs": {"shutdown": "wraps up"}}
    if verb == "shutdown":
        _shut_calls.append(id)
        # the shutdown dispatch may still use its tools: a nested send passes the drain
        r = (await kernel.send([{"target": "kernel", "payload": {"type": "ps"}}]))[0]
        return {"wrapped": True, "tools_ok": r["status"] == "completed"}
    if verb == "slow":
        _run_flag.setdefault(id, asyncio.Event()).set()
        await asyncio.sleep(30)
        return {"finished": True}
    if verb == "work":
        return {"ok": True}
    if verb == "count":
        _counts[id] = _counts.get(id, 0) + 1
        return {"n": _counts[id]}
    if verb == "fail":
        raise RuntimeError("probe failed on purpose")
    return {"error": f"unknown {verb!r}", "reason": "unhandled"}


_mod = types.ModuleType("mem_probe.tools")
_mod.handler = _handler
_mod.VERBS = {"reflect": None, "shutdown": None, "slow": None, "work": None}
sys.modules["mem_probe.tools"] = _mod


async def _call(root, target, payload, **item):
    return (await root.send([{"target": target, "payload": payload, **item}]))[0]


async def _mk(root, aid="probe", **meta):
    r = (
        await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": aid,
                "handler_module": "mem_probe.tools",
                "callable_by": {"$parent": "*", "rest": "*", "caller": "*"},
                **meta,
            },
        )
    )["result"]
    assert r.get("id") == aid, r
    return aid


async def _with_store(root, *, operator=True, start=True):
    """A ram_store `memory` under the root, named by the root; the kernel started."""
    r = (
        await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": "memory",
                "handler_module": "ram_store.tools",
                "operator": operator,
                "admins": ["kernel"],
            },
        )
    )["result"]
    assert r.get("id") == "memory", r
    root.ctx.update(root.id, memory_store_id="memory")
    if start:
        out = await root.ctx.start(boot_all=False)
        assert out.get("started"), out
    return "memory"


async def _kernel_rows(root) -> list[dict]:
    tables = (await _call(root, "memory", {"type": "dump"}))["result"]["tables"]
    guid = root.record["guid"]
    return [r["row"] for r in tables["rows"] if r["owner"] == f"{guid}|kernel"]


# ── the tail ────────────────────────────────────────────────────────────


async def test_rows_stream_replays_after_then_tails_without_duplicates(seeded_kernel):
    k = seeded_kernel.ctx
    a = k.rows_put("probe", {"kind": "call", "verb": "one"})
    b = k.rows_put("probe", {"kind": "call", "verb": "two"})
    got: list[int] = []

    async def consume():
        async for row in k.rows_stream(a):
            got.append(row["id"])
            if len(got) == 3:
                return

    t = asyncio.create_task(consume())
    await asyncio.sleep(0.01)
    c = k.rows_put("probe", {"kind": "call", "verb": "three"})
    d = k.rows_put("other", {"kind": "call", "verb": "four"})
    await asyncio.wait_for(t, timeout=2)
    assert got == [b, c, d], "replay (> after) then live, once each"
    assert k._tails == [], "a consumer that stopped is dropped"


# ── the drain and the agents half of a stop ─────────────────────────────


async def test_draining_refuses_new_roots_but_not_shutdown_traffic(seeded_kernel):
    a = await _mk(seeded_kernel)
    k = seeded_kernel.ctx
    k.draining = True
    try:
        out = await _call(seeded_kernel, a, {"type": "work"})
        assert out["status"] == "failed" and out["result"]["reason"] == "draining"
        assert out["pid"] is None, "a refused root costs no id"
        with shutdown_context():
            out = await _call(seeded_kernel, a, {"type": "work"})
        assert out["status"] == "completed"
    finally:
        k.draining = False


async def test_shutdown_agents_tells_advertisers_waits_then_cancels_the_rest(
    seeded_kernel,
):
    _shut_calls.clear()
    a = await _mk(seeded_kernel, "sp_a")
    b = await _mk(seeded_kernel, "sp_b")
    k = seeded_kernel.ctx
    k.QUIET_TIMEOUT = 0.3
    slow = asyncio.create_task(
        seeded_kernel.send([{"target": b, "payload": {"type": "slow"}}])
    )
    await asyncio.wait_for(_run_flag.setdefault(b, asyncio.Event()).wait(), timeout=2)
    kid = k.spawn_task(seeded_kernel.id, "persister", asyncio.sleep(30), keepalive=True)
    keep = k.tasks[kid]["_task"]
    out = (await _call(seeded_kernel, "kernel", {"type": "shutdown_agents"}))["result"]
    assert out.get("drained") is True and sorted(out["shut"]) == [a, b], out
    assert set(_shut_calls) == {a, b}, "every advertiser was told"
    assert out["cancelled"], "the slow task was cancelled after the quiet bound"
    assert kid in k.tasks, "the keepalive loop is not pending work"
    settled = await asyncio.wait_for(slow, timeout=2)
    assert settled[0]["status"] == "cancelled"
    r = await _call(seeded_kernel, a, {"type": "work"})
    assert r["result"]["reason"] == "draining"
    keep.cancel()
    k.draining = False


# ── kernel_start: no store, no start ───────────────────────────────────


async def test_kernel_start_fails_loud_without_a_store(seeded_kernel):
    out = (await _call(seeded_kernel, "kernel", {"type": "kernel_start"}))["result"]
    assert out["reason"] == "missing_arg" and out["arg"] == "memory_store_id", out
    assert "ram_store" in out["hint"], "the refusal says what to create"
    assert seeded_kernel.ctx.started is False


async def test_kernel_start_refuses_a_store_that_does_not_serve_the_operator(
    seeded_kernel,
):
    await _with_store(seeded_kernel, operator=False, start=False)
    out = await seeded_kernel.ctx.start(boot_all=False)
    assert out["reason"] == "unauthorized" and "operator" in out["hint"], out
    assert seeded_kernel.ctx.started is False


async def test_kernel_start_boots_the_store_first_then_everyone(seeded_kernel):
    await _mk(seeded_kernel)
    await _with_store(seeded_kernel, start=False)
    out = (await _call(seeded_kernel, "kernel", {"type": "kernel_start"}))["result"]
    assert out["started"] and out["store"] == "memory"
    assert out["booted"][:2] == [seeded_kernel.id, "memory"], out
    assert "probe" in out["booted"]
    again = (await _call(seeded_kernel, "kernel", {"type": "kernel_start"}))["result"]
    assert again.get("already") is True


# ── the flusher ────────────────────────────────────────────────────────


async def test_execution_rows_flush_into_the_store_and_leave_ram(seeded_kernel):
    await _mk(seeded_kernel)
    await _with_store(seeded_kernel)
    out = await _call(seeded_kernel, "probe", {"type": "work"})
    pid = out["pid"]
    await seeded_kernel.ctx.flush_executions()
    rows = await _kernel_rows(seeded_kernel)
    call = next(r for r in rows if r["id"] == pid)
    assert call["kind"] == "call" and call["caller"] is None
    settle = next(r for r in rows if r.get("kind") == "settle" and r["parent"] == pid)
    # UNNAMED: the settle keeps a digest, never the body (the growth fix, §3)
    assert settle["outcome"] == {"status": "completed", "bytes": len('{"ok": true}')}
    assert any(r.get("kind") == "flush" and pid in r["covers"] for r in rows)
    assert pid not in seeded_kernel.ctx.rows, "flushed rows leave RAM"
    assert pid not in seeded_kernel.ctx.executions, "a settled, flushed execution too"


async def test_stored_rows_keep_none_and_reduce_bytes_to_a_length(seeded_kernel):
    """C25 fidelity: a call's payload lands in the store verbatim — None stays None —
    except raw bytes, recorded as `{len: N}` (base64 is banned). And `dump` → `inject`
    round-trips the tables unchanged."""
    await _mk(seeded_kernel)
    await _with_store(seeded_kernel)
    out = await _call(
        seeded_kernel, "probe", {"type": "work", "blob": b"\x00\x01\x02", "none": None}
    )
    await seeded_kernel.ctx.flush_executions()
    call = next(r for r in await _kernel_rows(seeded_kernel) if r["id"] == out["pid"])
    assert call["payload"]["blob"] == {"len": 3}
    assert "none" in call["payload"] and call["payload"]["none"] is None
    before = (await _call(seeded_kernel, "memory", {"type": "dump"}))["result"][
        "tables"
    ]
    await _call(seeded_kernel, "memory", {"type": "inject", "tables": before})
    after = (await _call(seeded_kernel, "memory", {"type": "dump"}))["result"]["tables"]
    # the flusher may land this test's own dump/inject calls in between — appended after
    assert after["rows"][: len(before["rows"])] == before["rows"]
    assert (
        after["fold_refs"] == before["fold_refs"]
        and after["grants"] == before["grants"]
    )


async def test_a_settle_keeps_the_body_only_when_named_or_the_error_when_dead(
    seeded_kernel,
):
    """What a settle row keeps: a NAMED call's whole result (the dedup contract), a
    dead letter's error text, and `{status, bytes}` for everything else — so the
    kernel's store reads, recorded as executions, never re-record the store."""
    await _mk(seeded_kernel)
    await _with_store(seeded_kernel)
    named = await _call(seeded_kernel, "probe", {"type": "work"}, id="n-1")
    plain = await _call(seeded_kernel, "probe", {"type": "work"})
    dead = await _call(seeded_kernel, "probe", {"type": "fail"})
    await seeded_kernel.ctx.flush_executions()
    rows = await _kernel_rows(seeded_kernel)
    kept = {r["parent"]: r["outcome"] for r in rows if r.get("kind") == "settle"}
    assert kept[named["pid"]] == {"status": "completed", "result": {"ok": True}}
    assert kept[plain["pid"]] == {"status": "completed", "bytes": 12}
    assert kept[dead["pid"]]["status"] == "failed"
    assert "probe failed on purpose" in kept[dead["pid"]]["error"]
    assert "result" not in kept[dead["pid"]]


async def test_repeated_ledger_calls_grow_the_store_linearly(seeded_kernel):
    """Measured before the fix: ~6x per `ledger` call (2 KB → 14.8 MB in five)."""
    import json as _json

    await _mk(seeded_kernel)
    await _with_store(seeded_kernel)

    async def size() -> int:
        await seeded_kernel.ctx.flush_executions()
        t = (await _call(seeded_kernel, "memory", {"type": "dump"}))["result"]
        return len(_json.dumps(t["tables"], default=str))

    sizes = [await size()]
    for _ in range(6):
        await _call(seeded_kernel, "kernel", {"type": "ledger"})
        sizes.append(await size())
    steps = [b - a for a, b in zip(sizes, sizes[1:])]
    assert max(steps[1:]) < 2 * steps[1], f"not linear: {sizes}"


async def test_the_flush_records_itself_and_never_loops(seeded_kernel):
    await _mk(seeded_kernel)
    await _with_store(seeded_kernel)
    await _call(seeded_kernel, "probe", {"type": "work"})
    await asyncio.sleep(0.6)  # the flusher's own debounce, several times over
    n1 = len(await _kernel_rows(seeded_kernel))
    await asyncio.sleep(0.6)
    rows = await _kernel_rows(seeded_kernel)
    # the dumps above are operator reads — calls like any other — so the count may
    # grow by THEIR rows; what must not appear is a flush that only carries flushes
    flushes = [r for r in rows if r.get("kind") == "flush"]
    by_id = {r["id"]: r for r in rows}
    for f in flushes:
        covered = [by_id.get(i) for i in f["covers"] if i in by_id]
        assert not all(
            c is not None
            and c.get("kind") == "call"
            and (c.get("payload") or {}).get("type") == "append"
            for c in covered
        ), f"a flush that only re-records flushes — the loop is back: {f}"
    assert len(rows) >= n1


async def test_ledger_is_a_projection_of_store_and_ram_scoped_to_the_caller(
    seeded_kernel,
):
    await _mk(seeded_kernel)
    await _mk(
        seeded_kernel,
        "caller",
        can_call={"probe": "*", seeded_kernel.id: ["ledger", "ack_deadletter"]},
    )
    await _with_store(seeded_kernel)
    with sender_context("caller"):
        mine = await _call(seeded_kernel, "probe", {"type": "work"}, id="mine-1")
    other = await _call(seeded_kernel, "probe", {"type": "work"})
    await seeded_kernel.ctx.flush_executions()
    live = asyncio.create_task(
        seeded_kernel.send([{"target": "probe", "payload": {"type": "slow"}}])
    )
    await asyncio.wait_for(_run_flag.setdefault("probe", asyncio.Event()).wait(), 2)
    led = (await _call(seeded_kernel, "kernel", {"type": "ledger"}))["result"]
    by = {e["id"]: e for e in led["executions"]}
    assert by[mine["pid"]]["status"] == "completed", "read back from the store"
    assert by[mine["pid"]]["outcome"]["result"] == {"ok": True}, "named: kept"
    assert "result" not in by[other["pid"]]["outcome"], "unnamed: a digest"
    assert by[other["pid"]]["outcome"]["bytes"] == len('{"ok": true}')
    assert any(e["status"] == "in_flight" for e in led["executions"]), "RAM overlay"
    with sender_context("caller"):
        scoped = (await _call(seeded_kernel, "kernel", {"type": "ledger"}))["result"]
    ids = {e["id"] for e in scoped["executions"]}
    assert mine["pid"] in ids and other["pid"] not in ids
    live.cancel()


async def test_named_dedup_survives_the_flush(seeded_kernel):
    await _mk(seeded_kernel)
    await _with_store(seeded_kernel)
    _counts.clear()
    first = await _call(seeded_kernel, "probe", {"type": "count"}, id="job-1")
    await seeded_kernel.ctx.flush_executions()
    assert "job-1" not in seeded_kernel.ctx.names, "the name left RAM with its rows"
    again = await _call(seeded_kernel, "probe", {"type": "count"}, id="job-1")
    assert again["result"] == first["result"] == {"n": 1}
    assert _counts["probe"] == 1, "the step never ran twice"


async def test_named_dedup_survives_a_restart_on_the_same_store(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    await _call(root, root.id, {"type": "boot"})  # mints the guid
    await _mk(root)
    await _with_store(root)
    _counts.clear()
    await _call(root, "probe", {"type": "count"}, id="nightly")
    await root.ctx.flush_executions()
    tables = (await _call(root, "memory", {"type": "dump"}))["result"]["tables"]
    persist(root)
    # a NEW kernel on the same dir: same guid, same counter bound, an empty RAM store
    root2 = boot_root()
    assert root2.record["guid"] == root.record["guid"]
    assert root2.ctx.next_id >= root.ctx.next_id, "ids never repeat across a restart"
    await _call(root2, "memory", {"type": "inject", "tables": tables})
    out = await root2.ctx.start(boot_all=False)
    assert out.get("started"), out
    again = await _call(root2, "probe", {"type": "count"}, id="nightly")
    assert again["result"] == {"n": 1} and _counts["probe"] == 1


# ── dead letters ───────────────────────────────────────────────────────


async def test_a_dead_letter_is_announced_listed_and_acked(seeded_kernel):
    await _mk(seeded_kernel)
    await _mk(
        seeded_kernel,
        "caller",
        can_call={"probe": "*", seeded_kernel.id: ["ledger", "ack_deadletter"]},
    )
    await _with_store(seeded_kernel)
    inbox = seeded_kernel.ctx.ensure_inbox("caller")
    with sender_context("caller"):
        out = await _call(seeded_kernel, "probe", {"type": "fail"})
    assert out["status"] == "failed"
    ev = inbox.get_nowait()
    assert ev["type"] == "on_deadletter" and ev["id"] == out["pid"], ev
    await seeded_kernel.ctx.flush_executions()
    with sender_context("caller"):
        led = (await _call(seeded_kernel, "kernel", {"type": "ledger"}))["result"]
        entry = next(e for e in led["executions"] if e["id"] == out["pid"])
        assert entry["status"] == "failed" and not entry.get("acked")
        acked = (
            await _call(
                seeded_kernel, "kernel", {"type": "ack_deadletter", "id": out["pid"]}
            )
        )["result"]
        assert acked == {"acked": True, "id": out["pid"]}
        ok = await _call(seeded_kernel, "probe", {"type": "work"})
        refused = (
            await _call(
                seeded_kernel, "kernel", {"type": "ack_deadletter", "id": ok["pid"]}
            )
        )["result"]
        assert refused["reason"] == "bad_arg", "only a dead letter is acked"
    await seeded_kernel.ctx.flush_executions()
    led = (await _call(seeded_kernel, "kernel", {"type": "ledger"}))["result"]
    assert next(e for e in led["executions"] if e["id"] == out["pid"])["acked"] is True
    rows = await _kernel_rows(seeded_kernel)
    assert any(r.get("kind") == "ack" and r["parent"] == out["pid"] for r in rows)


async def test_a_graceful_stop_lands_in_flight_work_as_cancelled_dead_letters(
    seeded_kernel,
):
    await _mk(seeded_kernel)
    await _with_store(seeded_kernel)
    k = seeded_kernel.ctx
    k.QUIET_TIMEOUT = 0.2
    slow = asyncio.create_task(
        seeded_kernel.send([{"target": "probe", "payload": {"type": "slow"}}])
    )
    await asyncio.wait_for(_run_flag.setdefault("probe", asyncio.Event()).wait(), 2)
    pid = next(t["id"] for t in k.task_rows() if t["verb"] == "slow")
    tables_before_walk: dict = {}
    real = seeded_kernel.shutdown

    async def capture():
        tables_before_walk.update(
            (await _call(seeded_kernel, "memory", {"type": "dump"}))["result"]["tables"]
        )
        await real()

    seeded_kernel.shutdown = capture
    with shutdown_context():
        await k.shutdown()
    await asyncio.wait_for(slow, 2)
    guid = seeded_kernel.record["guid"]
    rows = [
        r["row"] for r in tables_before_walk["rows"] if r["owner"] == f"{guid}|kernel"
    ]
    settle = next(r for r in rows if r.get("kind") == "settle" and r["parent"] == pid)
    assert settle["outcome"]["status"] == "cancelled", "a dead letter, flushed"


# ── retention: the store-side window ───────────────────────────────────


async def test_the_kernels_store_is_pruned_to_its_window(seeded_kernel):
    """`ledger_cap` is the store-side window: over it, the oldest settled executions
    go — unnamed first — while a named one (its dedup contract) and an unacked dead
    letter survive."""
    await _mk(seeded_kernel)
    await _with_store(seeded_kernel)
    k = seeded_kernel.ctx
    named = await _call(seeded_kernel, "probe", {"type": "work"}, id="keep-me")
    dead = await _call(seeded_kernel, "probe", {"type": "fail"})
    for _ in range(40):
        await _call(seeded_kernel, "probe", {"type": "work"})
    k.ledger_cap = 20
    await k.flush_executions()
    rows = await _kernel_rows(seeded_kernel)
    calls = [r for r in rows if r.get("kind") == "call" and not r.get("internal")]
    ids = {r["id"] for r in calls}
    assert len(calls) <= 22, (
        f"the store holds {len(calls)} calls against a window of 20"
    )
    assert named["pid"] in ids, (
        "a named execution is kept — it carries a dedup contract"
    )
    assert dead["pid"] in ids, "an unacked dead letter waits for a person"
    settles = {r["parent"] for r in rows if r.get("kind") == "settle"}
    assert ids <= settles | {r["id"] for r in calls if r["id"] in k.executions}, (
        "no settle row outlives its call and no call loses its settle"
    )


# ── the id counter: block reservation in the root record ───────────────


async def test_the_id_counter_reserves_blocks_in_the_root_record(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    assert "next_id" not in root.record, "load never edits a record"
    await _call(root, root.id, {"type": "boot"})
    await _with_store(root)
    n = root.ctx.mint_id()
    bound = root.record.get("next_id")
    assert bound is not None and bound > n, "start wrote the reserved bound"
    for _ in range(root.ctx.ID_BLOCK):
        root.ctx.mint_id()
    assert root.record["next_id"] > bound, "a started kernel persists the next block"
    bound = root.record["next_id"]
    persist(root)
    on_disk = yaml.safe_load((Path(".fantastic") / "agent.yaml").read_text())
    assert on_disk["next_id"] == bound
    root2 = boot_root()
    assert root2.ctx.mint_id() >= bound, "a reboot resumes past the reserved block"


async def test_kernel_stop_acks(seeded_kernel):
    out = (await _call(seeded_kernel, "kernel", {"type": "kernel_stop"}))["result"]
    assert out == {"type": "kernel_stop", "ok": True}
