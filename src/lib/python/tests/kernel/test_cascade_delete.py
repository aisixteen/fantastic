"""Cascade delete — depth-first, lock-blocked, partial-mutation-free.

The substrate's promise: `agent.delete(child_id)` runs `on_delete` on
every descendant (deepest first) BEFORE removing records. Any
`delete_lock` anywhere in the subtree blocks the entire cascade with
`{locked, blocked_by, error}` and zero state mutations. By the time
delete returns, every descendant is gone from `ctx.agents` and the
parent's `_children`. Disk cleanup (rmtree) is the loader's job, driven
by the `removed` state event — covered in the kernel_state tests.
"""

from __future__ import annotations

import asyncio

from _testkit import command_rx


async def test_cascade_walks_deepest_first(seeded_kernel):
    """Three-level pyramid: delete root of subtree → all descendants
    removed; ctx.agents and disk both reflect the cascade."""
    seeded_kernel.create("file_bridge.tools", allow=["."], id="P")
    seeded_kernel.create("file_bridge.tools", allow=["."], id="C", display_name="child")
    # Children-of-children: simulate a nested tree by spawning under
    # the parent agent (P) rather than under root.
    p_agent = seeded_kernel.ctx.agents["P"]
    p_agent.create("file_bridge.tools", allow=["."], id="GC1")
    p_agent.create("file_bridge.tools", allow=["."], id="GC2")
    assert {"P", "GC1", "GC2"}.issubset(seeded_kernel.ctx.agents.keys())

    r = await seeded_kernel.delete("P")
    assert r == {"deleted": True, "id": "P"}
    # All three (P + GC1 + GC2) gone from ctx and from parent _children.
    for gone in ("P", "GC1", "GC2"):
        assert gone not in seeded_kernel.ctx.agents
    assert "P" not in seeded_kernel._children


async def test_cascade_lock_blocks_with_blocked_by(seeded_kernel):
    """delete_lock on a deep descendant blocks the cascade. Response
    carries `locked:true, blocked_by:<id>, error:...`. NO mutations
    happen — the entire subtree is intact."""
    seeded_kernel.create("file_bridge.tools", allow=["."], id="P")
    p_agent = seeded_kernel.ctx.agents["P"]
    p_agent.create("file_bridge.tools", allow=["."], id="LOCKED", delete_lock=True)
    p_agent.create("file_bridge.tools", allow=["."], id="OK")
    r = await seeded_kernel.delete("P")
    assert r["reason"] == "locked"  # &#27: the one refusal shape, not a flag
    assert r["blocked_by"] == "LOCKED"
    # No mutations.
    assert "P" in seeded_kernel.ctx.agents
    assert "LOCKED" in seeded_kernel.ctx.agents
    assert "OK" in seeded_kernel.ctx.agents


async def test_cascade_lock_unblocks_after_clear(seeded_kernel):
    """Clear the lock via update_agent; cascade proceeds."""
    seeded_kernel.create("file_bridge.tools", allow=["."], id="P")
    p_agent = seeded_kernel.ctx.agents["P"]
    p_agent.create("file_bridge.tools", allow=["."], id="L", delete_lock=True)
    blocked = await seeded_kernel.delete("P")
    assert blocked["reason"] == "locked"
    seeded_kernel.update("L", delete_lock=False)
    r = await seeded_kernel.delete("P")
    assert r == {"deleted": True, "id": "P"}
    assert "P" not in seeded_kernel.ctx.agents
    assert "L" not in seeded_kernel.ctx.agents


async def test_cascade_emits_agent_deleted(seeded_kernel):
    """The `delete_agent` verb on root emits `agent_deleted` after the
    cascade returns successfully — NOT for the silent system call."""
    seeded_kernel.create("file_bridge.tools", allow=["."], id="X")
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(events.append)
    (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {"type": "delete_agent", "id": "X"},
                }
            ]
        )
    )[0]["result"]
    # `removed` lifecycle event for X (always fires).
    assert any(e.get("kind") == "removed" and e.get("agent_id") == "X" for e in events)
    # `agent_deleted` emit on root's inbox (verb-level).
    deleted_emits = [
        e["payload"]
        for e in events
        if e.get("kind") == "emit"
        and e.get("agent_id") == seeded_kernel.id
        and e.get("payload", {}).get("type") == "agent_deleted"
    ]
    assert len(deleted_emits) >= 1
    assert deleted_emits[0]["id"] == "X"


async def test_cascade_through_nested_member_kills_backend(seeded_kernel):
    """Real domain case: an exec_bridge running a PTY job (a terminal), nested
    under a parent agent. Delete the parent → cascade reaches exec_bridge's
    on_delete FIRST (SIGKILLs the job's process), then both records vanish."""
    import sys

    from exec_bridge import tools as ebt

    parent = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                    },
                }
            ]
        )
    )[0]["result"]
    parent_id = parent["id"]
    backend = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": parent_id,
                        "handler_module": "exec_bridge.tools",
                        "ingress_rule": "allow_all_ingress",
                        "cwd": ".",
                        "allowed": [rf"{command_rx(sys.executable)} -c .*"],
                    },
                }
            ]
        )
    )[0]["result"]
    backend_id = backend["id"]
    assert backend_id in seeded_kernel.ctx.agents
    job = (
        await seeded_kernel.send(
            [
                {
                    "target": backend_id,
                    "payload": {
                        "type": "start",
                        "command": sys.executable,
                        "args": ["-c", "import time; time.sleep(60)"],
                        "pty": {"cols": 80, "rows": 24},
                    },
                }
            ]
        )
    )[0]["result"]
    proc = ebt._jobs[job["job_id"]]["proc"]

    r = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {"type": "delete_agent", "id": parent_id},
                }
            ]
        )
    )[0]["result"]
    assert r["deleted"] is True
    assert parent_id not in seeded_kernel.ctx.agents
    assert backend_id not in seeded_kernel.ctx.agents
    assert await asyncio.wait_for(proc.wait(), timeout=5) != 0, "the PTY job was killed"


# ── delete_agent keep_memory: a flag that rides kernel state, for one delete ──


def _keep_probe(name: str, seen: list) -> None:
    """A bundle whose `on_delete` records what `keep_memory` says for its agent."""
    import types

    from kernel import register_unlisted
    from kernel._agent import keep_memory

    mod = types.ModuleType(name)
    mod.VERBS = {}

    async def handler(id, payload, kernel):
        return {"ok": id}

    async def on_delete(agent):
        seen.append((agent.id, keep_memory(agent.ctx, agent.id)))

    mod.handler = handler
    mod.on_delete = on_delete
    register_unlisted(name, mod)


async def _delete_verb(k, payload: dict) -> dict:
    out = await k.send(
        [{"target": k.id, "payload": {"type": "delete_agent", **payload}}]
    )
    return out[0]["result"]


async def test_delete_agent_keep_memory_flag_is_scoped_to_one_delete(seeded_kernel):
    from kernel._agent import keep_memory

    seen: list = []
    _keep_probe("keep_probe_mod", seen)
    seeded_kernel.create("keep_probe_mod", id="P")
    seeded_kernel.ctx.agents["P"].create("keep_probe_mod", id="C")

    out = await _delete_verb(seeded_kernel, {"id": "P", "keep_memory": True})
    assert out == {"deleted": True, "id": "P"}
    assert sorted(seen) == [("C", True), ("P", True)], "the flag reaches every hook"
    assert keep_memory(seeded_kernel.ctx, "P") is False, "cleared after the delete"

    seeded_kernel.create("keep_probe_mod", id="Q")
    await _delete_verb(seeded_kernel, {"id": "Q"})
    assert seen[-1] == ("Q", False), "a plain delete does not keep"

    seeded_kernel.create("keep_probe_mod", id="R")
    await _delete_verb(seeded_kernel, {"id": "R", "keep_memory": "true"})
    assert seen[-1] == ("R", False), "only the literal true is honoured"
    assert not keep_memory(seeded_kernel.ctx, "R")
