"""Persistence across kernel reboot.

Records persisted by the loader survive `Agent` destruction. A fresh
bootstrap (`boot_root()`) in the same `.fantastic/` directory rehydrates
the entire tree — same ids, same parent-child links, same meta — without
re-running `create_agent`. Process-memory state (PTY child, uvicorn task,
in-flight requests) does NOT survive; bundles' `_boot` respawns it.

`Agent` no longer self-persists; tests call `persist()` (a synchronous
full flush via the loader) to materialize on-disk state before rebooting,
mirroring what the live debounced flush does in a running daemon.
"""

from __future__ import annotations

import json

from _testkit import boot_root, persist


async def test_top_level_agent_rehydrates(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    k1 = boot_root()
    rec = await k1.send(
        k1.id,
        {"type": "create_agent", "handler_module": "file_bridge.tools", "x": 7},
    )
    aid = rec["id"]
    assert aid in k1.ctx.agents
    persist(k1)
    # Drop k1, re-bootstrap from same dir.
    del k1
    k2 = boot_root()
    assert aid in k2.ctx.agents
    assert k2.get(aid)["x"] == 7


async def test_nested_pair_rehydrates(tmp_path, monkeypatch):
    """A nested parent→child pair survives a reboot paired (same ids, same
    parent-child link). Structure rehydrates from disk position."""
    monkeypatch.chdir(tmp_path)
    k1 = boot_root()
    parent = await k1.send(
        k1.id, {"type": "create_agent", "handler_module": "file_bridge.tools"}
    )
    parent_id = parent["id"]
    child = await k1.send(
        parent_id, {"type": "create_agent", "handler_module": "file_bridge.tools"}
    )
    child_id = child["id"]
    persist(k1)

    del k1
    k2 = boot_root()
    # Same ids, same parent-child link.
    assert parent_id in k2.ctx.agents
    assert child_id in k2.ctx.agents
    assert child_id in k2.ctx.agents[parent_id]._children


async def test_persistence_survives_corrupted_sibling(tmp_path, monkeypatch):
    """A corrupted agent.json under `agents/X/` is silently skipped;
    other agents in the same dir hydrate fine."""
    monkeypatch.chdir(tmp_path)
    k1 = boot_root()
    rec = await k1.send(
        k1.id, {"type": "create_agent", "handler_module": "file_bridge.tools"}
    )
    good_id = rec["id"]
    persist(k1)
    # Plant a corrupt sibling.
    bad = tmp_path / ".fantastic" / "agents" / "broken"
    bad.mkdir()
    (bad / "agent.json").write_text("{NOT_JSON")
    del k1
    k2 = boot_root()
    assert good_id in k2.ctx.agents
    assert "broken" not in k2.ctx.agents


async def test_reboot_resets_in_flight_counters(tmp_path, monkeypatch):
    """Process-memory state (in_flight, inboxes, _watcher_ids) does
    NOT persist. Only records do."""
    monkeypatch.chdir(tmp_path)
    k1 = boot_root()
    rec = await k1.send(
        k1.id, {"type": "create_agent", "handler_module": "file_bridge.tools"}
    )
    aid = rec["id"]
    persist(k1)
    # Bump in_flight artificially then reboot.
    k1.ctx.agents[aid]._in_flight = 5
    del k1
    k2 = boot_root()
    assert k2.ctx.agents[aid]._in_flight == 0


def test_agent_record_roundtrip_on_disk(tmp_path, monkeypatch):
    """The on-disk record carries id + handler_module + parent_id +
    arbitrary meta. The exact JSON shape is the contract."""
    monkeypatch.chdir(tmp_path)
    k = boot_root()
    k.create("file_bridge.tools", id="ondisk", x=1, y=2, display_name="d")
    persist(k)
    af = tmp_path / ".fantastic" / "agents" / "ondisk" / "agent.json"
    rec = json.loads(af.read_text())
    assert rec["id"] == "ondisk"
    assert rec["handler_module"] == "file_bridge.tools"
    assert rec["parent_id"] == "kernel_state"
    assert rec["x"] == 1 and rec["y"] == 2
    assert rec["display_name"] == "d"
