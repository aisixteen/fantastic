"""kernel_state.persist_blob / load_blob — the loader as the ONE byte-store authority.

A dumb whole-file authority: any agent persists its sidecars THROUGH the loader's
discovered store instead of wiring its own file_bridge_id. NO data semantics, NO RAM
fallback, and a filename guard against clobbering kernel-managed files / escaping the
agent's own dir."""

from pathlib import Path

import pytest


async def _mk_target(kernel, mode="mem"):
    """A live child agent to own the sidecars (yaml_state is the real consumer)."""
    rec = await kernel.send(
        kernel.id,
        {"type": "create_agent", "handler_module": "yaml_state.tools", "mode": mode},
    )
    assert "id" in rec, rec
    return rec["id"]


async def test_persist_then_load_round_trips(seeded_kernel, store_agent):
    aid = await _mk_target(seeded_kernel)
    w = await seeded_kernel.send(
        "kernel_state",
        {
            "type": "persist_blob",
            "agent_id": aid,
            "name": "state.yaml",
            "content": "user.name: Ada\n",
        },
    )
    assert w.get("ok") is True, w
    # Lands on disk store-relative, next to the agent's record dir.
    on_disk = Path(f".fantastic/agents/{aid}/state.yaml")
    assert on_disk.exists(), "blob must land under .fantastic/agents/<id>/"
    assert "Ada" in on_disk.read_text()
    r = await seeded_kernel.send(
        "kernel_state", {"type": "load_blob", "agent_id": aid, "name": "state.yaml"}
    )
    assert r.get("content") == "user.name: Ada\n"


async def test_load_absent_is_null_not_error(seeded_kernel, store_agent):
    aid = await _mk_target(seeded_kernel)
    r = await seeded_kernel.send(
        "kernel_state", {"type": "load_blob", "agent_id": aid, "name": "missing.yaml"}
    )
    assert r.get("content") is None  # never-written sidecar reads empty
    assert "error" not in r


@pytest.mark.parametrize(
    "bad", ["agent.json", "readme.md", "../escape", "sub/dir.yaml", ".hidden"]
)
async def test_guard_rejects_dangerous_names(seeded_kernel, store_agent, bad):
    aid = await _mk_target(seeded_kernel)
    w = await seeded_kernel.send(
        "kernel_state",
        {"type": "persist_blob", "agent_id": aid, "name": bad, "content": "x"},
    )
    assert "error" in w, f"guard must reject {bad!r}: {w!r}"


async def test_no_store_failfasts_no_ram(seeded_kernel):
    # No file_bridge rooted at .fantastic ⇒ no store discovered ⇒ error (no RAM).
    aid = await _mk_target(seeded_kernel)
    w = await seeded_kernel.send(
        "kernel_state",
        {"type": "persist_blob", "agent_id": aid, "name": "state.yaml", "content": "x"},
    )
    assert "error" in w and "no store" in w["error"], w


async def test_sealed_store_surfaces_denied_write(seeded_kernel):
    # A store rooted at .fantastic but SEALED (no ingress_rule) is still discovered;
    # the denied write is SURFACED, not silently dropped (no fallback).
    sealed = await seeded_kernel.send(
        seeded_kernel.id,
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "root": ".fantastic",
        },
    )
    assert "id" in sealed, sealed
    aid = await _mk_target(seeded_kernel)
    w = await seeded_kernel.send(
        "kernel_state",
        {"type": "persist_blob", "agent_id": aid, "name": "state.yaml", "content": "x"},
    )
    assert "error" in w, w
    assert w.get("reason") == "unauthorized", w
