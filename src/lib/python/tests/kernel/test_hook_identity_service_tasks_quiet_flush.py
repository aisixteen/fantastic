"""The task table and the flusher.

- Every `on_cancel` hook — the `cancel` verb's path and an item's own
  deadline alike — and every `on_delete` hook runs AS THE AGENT ITSELF (its own id
  is the sender), never as the operator or the root.
- `cancel` refuses the kernel's own service tasks (the execution flusher,
  the record persister — keepalive loops the root owns); `ps` still lists them,
  and a group kill leaves them out.
- The kernel's own ledger-flush send publishes NOTHING on the state stream
  (its rows stay recorded by the flush row) — a long-poll waiter is not woken.
"""

from __future__ import annotations

import asyncio
import types

from kernel import register_unlisted
from kernel._kernel import _current_sender

_started: dict[str, asyncio.Event] = {}
_hook_senders: list[tuple[str, str, str | None]] = []


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "hook identity probe", "verbs": {}}
    if verb == "slow":
        _started.setdefault(id, asyncio.Event()).set()
        await asyncio.sleep(30)
    if verb == "fork_service":
        # a root-owned keepalive loop forked INSIDE this dispatch: it is in this
        # task's process group by parentage
        ctx = kernel.ctx
        tid = ctx.spawn_task(
            ctx.root.id, "svc_probe", asyncio.sleep(30), keepalive=True
        )
        _started.setdefault(id, asyncio.Event()).set()
        await asyncio.sleep(30)
        return {"svc": tid}
    return {"ok": id}


async def _on_cancel(id, task_id, kernel):
    _hook_senders.append(("cancel", id, _current_sender.get()))
    # an emit on its OWN inbox — ai_core's final `status done` to a browser client
    await kernel.emit(id, {"type": "status", "phase": "done", "reason": "cancelled"})


async def _on_delete(agent):
    _hook_senders.append(("delete", agent.id, _current_sender.get()))


_mod = types.ModuleType("hook_identity_probe.tools")
_mod.handler = _handler
_mod.VERBS = {}
_mod.on_cancel = _on_cancel
_mod.on_delete = _on_delete
register_unlisted("hook_identity_probe.tools", _mod)


async def _call(root, target, payload, **item) -> dict:
    return (await root.send([{"target": target, "payload": payload, **item}]))[0]


async def _grandchild(root) -> str:
    """`gc` under `mid` under the root — NOT a direct child of the root."""
    for aid, parent in (("mid", root.id), ("gc", "mid")):
        r = (
            await _call(
                root,
                root.id,
                {
                    "type": "create_agent",
                    "parent_id": parent,
                    "id": aid,
                    "handler_module": "hook_identity_probe.tools",
                },
            )
        )["result"]
        assert r.get("id") == aid, r
    return "gc"


async def _start_slow(root, aid: str, **item) -> asyncio.Task:
    _started.pop(aid, None)
    t = asyncio.create_task(_call(root, aid, {"type": "slow"}, **item))
    await asyncio.wait_for(_started.setdefault(aid, asyncio.Event()).wait(), 2)
    return t


def _pid_of(root, aid: str, verb: str) -> int:
    return next(
        r["id"] for r in root.ctx.task_rows() if r["agent"] == aid and r["verb"] == verb
    )


# ── G-3: the hooks run as the agent itself ───────────────────────────────────


# twin: on_cancel_hook_runs_and_releases_its_resource
async def test_on_cancel_runs_as_the_agent_on_the_cancel_verb_path(seeded_kernel):
    root = seeded_kernel
    gc = await _grandchild(root)
    inbox = root.ctx.ensure_inbox(gc)
    _hook_senders.clear()
    pending = await _start_slow(root, gc)
    pid = _pid_of(root, gc, "slow")
    out = (await _call(root, "kernel", {"type": "cancel", "task_id": pid}))["result"]
    assert out == {"cancelled": True, "killed": [pid]}, out
    assert (await asyncio.wait_for(pending, 2))["status"] == "cancelled"
    assert _hook_senders == [("cancel", gc, gc)], _hook_senders
    # the own-inbox emit is delivered — never gated root → gc and dropped
    got = [inbox.get_nowait() for _ in range(inbox.qsize())]
    assert {"type": "status", "phase": "done", "reason": "cancelled"} in got, got


async def test_on_cancel_runs_as_the_agent_on_the_deadline_path(seeded_kernel):
    root = seeded_kernel
    gc = await _grandchild(root)
    inbox = root.ctx.ensure_inbox(gc)
    _hook_senders.clear()
    _started.pop(gc, None)
    out = await _call(root, gc, {"type": "slow"}, timeout=0.2)
    assert out["status"] == "timed_out", out
    assert _hook_senders == [("cancel", gc, gc)], _hook_senders
    got = [inbox.get_nowait() for _ in range(inbox.qsize())]
    assert {"type": "status", "phase": "done", "reason": "cancelled"} in got, got


# twin: kernel_bookkeeping_runs_the_handler_as_the_agent_with_no_origin
async def test_on_delete_runs_as_the_agent_itself(seeded_kernel):
    root = seeded_kernel
    await _grandchild(root)
    _hook_senders.clear()
    out = (await _call(root, root.id, {"type": "delete_agent", "id": "mid"}))["result"]
    assert out.get("deleted") is True, out
    # depth-first, each hook as its own agent — not the root that ran the verb
    assert _hook_senders == [("delete", "gc", "gc"), ("delete", "mid", "mid")]


# ── KV-5: the kernel's own service tasks are not cancellable ─────────────────


async def _started_kernel(root) -> None:
    r = (
        await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": "memory",
                "handler_module": "ram_store.tools",
            },
        )
    )["result"]
    assert r.get("id") == "memory", r
    root.ctx.update(root.id, memory_store_id="memory")
    out = await root.ctx.start(boot_all=False)
    assert out.get("started"), out


# twin: cancel_refuses_a_root_owned_keepalive_loop_and_ps_still_lists_it
async def test_cancel_refuses_the_flusher_and_it_keeps_flushing(seeded_kernel):
    root = seeded_kernel
    await _started_kernel(root)
    k = root.ctx
    fid = _pid_of(root, root.id, "flush_executions")
    out = await _call(root, "kernel", {"type": "cancel", "task_id": fid})
    assert out["status"] == "errored", out
    assert out["result"] == {
        "error": f"cancel: task {fid} (flush_executions) is a kernel service task "
        "— not cancellable",
        "reason": "bad_arg",
        "hint": "it runs for the life of the kernel and stays listed in ps; "
        "kernel_stop ends it",
        "arg": "task_id",
    }, out
    ps = (await _call(root, "kernel", {"type": "ps"}))["result"]["tasks"]
    assert any(t["id"] == fid for t in ps), "still listed"
    assert not k.tasks[fid]["_task"].done(), "still running"
    # and still flushing: a new row leaves RAM
    rid = k.rows_put("probe", {"kind": "call", "verb": "after"})
    for _ in range(100):
        if rid not in k.rows:
            break
        await asyncio.sleep(0.02)
    assert rid not in k.rows, "the flusher kept writing"


async def test_a_group_kill_leaves_a_service_task_alive(seeded_kernel):
    root = seeded_kernel
    k = root.ctx
    r = (
        await _call(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "id": "forker",
                "handler_module": "hook_identity_probe.tools",
            },
        )
    )["result"]
    assert r.get("id") == "forker", r
    _started.pop("forker", None)
    pending = asyncio.create_task(_call(root, "forker", {"type": "fork_service"}))
    await asyncio.wait_for(_started.setdefault("forker", asyncio.Event()).wait(), 2)
    pid = _pid_of(root, "forker", "fork_service")
    svc = _pid_of(root, root.id, "svc_probe")
    assert k.tasks[svc]["parent"] == pid, "the service loop is in the group"
    out = (await _call(root, "kernel", {"type": "cancel", "task_id": pid}))["result"]
    assert out == {"cancelled": True, "killed": [pid]}, out
    assert (await asyncio.wait_for(pending, 2))["status"] == "cancelled"
    assert svc in k.tasks and not k.tasks[svc]["_task"].done(), "left alive"
    k.tasks[svc]["_task"].cancel()


# ── CLI-B: the ledger flush is silent on the state stream ───────────────────


async def test_the_ledger_flush_publishes_nothing_on_the_state_stream(seeded_kernel):
    root = seeded_kernel
    await _started_kernel(root)
    k = root.ctx
    await k.flush_executions()  # start from an empty RAM
    seen: list[dict] = []
    woken = asyncio.Event()  # a long-poll `events` waiter

    def tap(evt: dict) -> None:
        seen.append(evt)
        woken.set()

    unsub = k.add_state_subscriber(tap)
    try:
        await _call(root, "kernel", {"type": "list_agents"})  # rows to flush
        seen.clear()
        woken.clear()
        assert k.rows, "there is something to flush"
        out = await k.flush_executions()
        assert out["flushed"] > 0, out
        assert seen == [], seen
        assert not woken.is_set(), "a flush wakes no waiter"
        # the flush is recorded: its own flush row is in the store
        dump = (await _call(root, "memory", {"type": "dump"}))["result"]["tables"]
        assert any(
            r["row"].get("kind") == "flush" and r["row"].get("id") == out["flush"]
            for r in dump["rows"]
        )
        # traffic outside the flush still publishes
        await _call(root, "kernel", {"type": "list_agents"})
        assert seen, "the stream is not muted, only the flush"
    finally:
        unsub()


# twin: a_block_reserved_inside_the_flush_is_quiet_and_reaches_the_loader
async def test_an_id_block_reserved_inside_the_flush_is_quiet(seeded_kernel):
    root = seeded_kernel
    await _started_kernel(root)
    k = root.ctx
    await k.flush_executions()
    seen: list[dict] = []
    woken = asyncio.Event()  # a long-poll `events` waiter

    def tap(evt: dict) -> None:
        seen.append(evt)
        woken.set()

    unsub = k.add_state_subscriber(tap)
    try:
        await _call(root, "kernel", {"type": "list_agents"})  # rows to flush
        # the block is spent: the flush row's id — the flush's one mint — crosses
        # the bound inside the silent flush
        while k.next_id < k.id_reserved:
            k.mint_id()
        seen.clear()
        woken.clear()
        before = k.id_reserved
        await k.flush_executions()
        assert k.id_reserved > before
        assert seen == [], seen  # the reservation inside the flush is quiet
        assert not woken.is_set(), "a reservation wakes no waiter"
        assert root.record["next_id"] == k.id_reserved
    finally:
        unsub()
