"""Cascade delete — depth-first, lock-blocked, partial-mutation-free.

The substrate's promise: `agent.delete(child_id)` runs `on_delete` on
every descendant (deepest first) BEFORE removing records. Any
`delete_lock` anywhere in the subtree blocks the entire cascade with
`{locked, blocked_by, error}` and zero state mutations. By the time
delete returns, every descendant is gone from `ctx.agents`, parent's
`_children`, and disk.
"""

from __future__ import annotations


async def test_cascade_walks_deepest_first(seeded_kernel):
    """Three-level pyramid: delete root of subtree → all descendants
    removed; ctx.agents and disk both reflect the cascade."""
    seeded_kernel.create("file.tools", id="P")
    seeded_kernel.create("file.tools", id="C", display_name="child")
    # Children-of-children: simulate a nested tree by spawning under
    # the parent agent (P) rather than under root.
    p_agent = seeded_kernel.ctx.agents["P"]
    p_agent.create("file.tools", id="GC1")
    p_agent.create("file.tools", id="GC2")
    assert {"P", "GC1", "GC2"}.issubset(seeded_kernel.ctx.agents.keys())

    r = await seeded_kernel.delete("P")
    assert r == {"deleted": True, "id": "P"}
    # All three (P + GC1 + GC2) gone from ctx.
    for gone in ("P", "GC1", "GC2"):
        assert gone not in seeded_kernel.ctx.agents
    # GC1, GC2 directories gone too (under P/agents/...).
    p_dir = p_agent._root_path
    assert not p_dir.exists()


async def test_cascade_lock_blocks_with_blocked_by(seeded_kernel):
    """delete_lock on a deep descendant blocks the cascade. Response
    carries `locked:true, blocked_by:<id>, error:...`. NO mutations
    happen — the entire subtree is intact."""
    seeded_kernel.create("file.tools", id="P")
    p_agent = seeded_kernel.ctx.agents["P"]
    p_agent.create("file.tools", id="LOCKED", delete_lock=True)
    p_agent.create("file.tools", id="OK")
    r = await seeded_kernel.delete("P")
    assert r["locked"] is True
    assert r["blocked_by"] == "LOCKED"
    # No mutations.
    assert "P" in seeded_kernel.ctx.agents
    assert "LOCKED" in seeded_kernel.ctx.agents
    assert "OK" in seeded_kernel.ctx.agents


async def test_cascade_lock_unblocks_after_clear(seeded_kernel):
    """Clear the lock via update_agent; cascade proceeds."""
    seeded_kernel.create("file.tools", id="P")
    p_agent = seeded_kernel.ctx.agents["P"]
    p_agent.create("file.tools", id="L", delete_lock=True)
    blocked = await seeded_kernel.delete("P")
    assert blocked["locked"] is True
    seeded_kernel.update("L", delete_lock=False)
    r = await seeded_kernel.delete("P")
    assert r == {"deleted": True, "id": "P"}
    assert "P" not in seeded_kernel.ctx.agents
    assert "L" not in seeded_kernel.ctx.agents


async def test_cascade_emits_agent_deleted(seeded_kernel):
    """The `delete_agent` verb on root emits `agent_deleted` after the
    cascade returns successfully — NOT for the silent system call."""
    seeded_kernel.create("file.tools", id="X")
    events: list[dict] = []
    seeded_kernel.add_state_subscriber(events.append)
    await seeded_kernel.send(seeded_kernel.id, {"type": "delete_agent", "id": "X"})
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


async def test_cascade_through_terminal_webapp_kills_backend(seeded_kernel):
    """Real domain case: terminal_webapp owns terminal_backend. Delete
    the webapp → cascade reaches terminal_backend's on_delete FIRST
    (kills PTY in real life), then both records vanish."""
    rec = await seeded_kernel.send(
        seeded_kernel.id,
        {"type": "create_agent", "handler_module": "terminal_webapp.tools"},
    )
    webapp_id = rec["id"]
    web_agent = seeded_kernel.ctx.agents[webapp_id]
    backend_id = next(iter(web_agent._children.keys()))
    assert backend_id in seeded_kernel.ctx.agents

    r = await seeded_kernel.send(
        seeded_kernel.id, {"type": "delete_agent", "id": webapp_id}
    )
    assert r["deleted"] is True
    assert webapp_id not in seeded_kernel.ctx.agents
    assert backend_id not in seeded_kernel.ctx.agents
