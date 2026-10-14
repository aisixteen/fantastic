"""The ROW STATE EVENT: every EXECUTION row the kernel puts
(`call`, `settle`, `task`, `ack`) is announced on the state stream as
`{agent_id, kind: "row", row: {id, kind, parent?, caller?, settled?}}` — content-
free, so a reader fetches rows only when told and polls nothing. Nothing inside
the kernel's own ledger flush is published: an idle kernel publishes nothing."""

from __future__ import annotations

import asyncio


async def _started_kernel(root) -> None:
    """A ram_store `memory` under the root, named by the root; the kernel started
    (its ledger flusher running)."""
    out = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "id": "memory",
                        "handler_module": "ram_store.tools",
                    },
                }
            ]
        )
    )[0]["result"]
    assert out.get("id") == "memory", out
    root.ctx.update(root.id, memory_store_id="memory")
    started = await root.ctx.start(boot_all=False)
    assert started.get("started"), started


def _rows(events: list[dict]) -> list[dict]:
    return [e for e in events if e.get("kind") == "row"]


async def test_a_call_announces_its_call_row_and_its_settle(seeded_kernel):
    root = seeded_kernel
    events: list[dict] = []
    unsub = root.add_state_subscriber(events.append)
    out = (await root.send([{"target": "kernel", "payload": {"type": "list_agents"}}]))[
        0
    ]
    unsub()
    pid = out["pid"]
    rows = _rows(events)
    call = next(e for e in rows if e["row"]["kind"] == "call" and e["row"]["id"] == pid)
    assert call["agent_id"] == root.id, call
    assert call["row"]["caller"] is None, "the operator's call"
    settle = next(
        e for e in rows if e["row"]["kind"] == "settle" and e["row"]["parent"] == pid
    )
    assert settle["agent_id"] == root.id and settle["row"]["settled"] == root.id
    # content-free: never a payload or an outcome
    for e in rows:
        assert set(e["row"]) <= {"id", "kind", "parent", "caller", "settled"}, e
        assert "ts" in e


async def test_a_forked_task_announces_its_task_row(seeded_kernel):
    root = seeded_kernel
    events: list[dict] = []
    unsub = root.add_state_subscriber(events.append)

    async def work():
        return None

    tid = root.ctx.spawn_task("cli", "probe_fork", work())
    unsub()
    task = next(e for e in _rows(events) if e["row"]["id"] == tid)
    assert task["agent_id"] == "cli" and task["row"]["kind"] == "task", task


async def test_the_flush_publishes_nothing_so_idle_is_silent(seeded_kernel):
    root = seeded_kernel
    await _started_kernel(root)
    k = root.ctx
    await asyncio.sleep(0.5)  # anything the start wrote is flushed
    events: list[dict] = []
    unsub = root.add_state_subscriber(events.append)
    rid = k.rows_put("probe", {"kind": "call", "verb": "after"})
    for _ in range(100):  # the flusher writes it to the store
        if rid not in k.rows:
            break
        await asyncio.sleep(0.02)
    assert rid not in k.rows, "never flushed"
    await asyncio.sleep(0.3)
    unsub()
    assert [e["row"]["id"] for e in _rows(events)] == [rid], (
        "the flush itself announced rows (or more events came while idle)"
    )
