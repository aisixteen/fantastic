"""Persistence across kernel reboot.

Records on disk survive `Agent` destruction. A fresh `Core(Kernel(), argv=[])`
in the same `.fantastic/` directory rehydrates the entire tree — same
ids, same parent-child links, same meta — without re-running
`create_agent`. Process-memory state (PTY child, uvicorn task, in-flight
requests) does NOT survive; bundles' `_boot` respawns it.
"""

from __future__ import annotations

import asyncio
import json

from core import Core
from kernel import Kernel


async def test_top_level_agent_rehydrates(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    k1 = Core(Kernel(), argv=[])
    rec = await k1.send(
        k1.id,
        {"type": "create_agent", "handler_module": "file.tools", "x": 7},
    )
    aid = rec["id"]
    assert aid in k1.ctx.agents
    # Drop k1, re-bootstrap from same dir.
    del k1
    k2 = Core(Kernel(), argv=[])
    assert aid in k2.ctx.agents
    assert k2.get(aid)["x"] == 7


async def test_nested_pair_rehydrates(tmp_path, monkeypatch):
    """terminal_webapp + its terminal_backend child survive a reboot
    paired (parent-child). On restart, terminal_webapp's `_boot` finds
    the existing backend child and skips creation."""
    monkeypatch.chdir(tmp_path)
    k1 = Core(Kernel(), argv=[])
    rec = await k1.send(
        k1.id, {"type": "create_agent", "handler_module": "terminal_webapp.tools"}
    )
    webapp_id = rec["id"]
    web_pre = k1.ctx.agents[webapp_id]
    backend_id_pre = next(iter(web_pre._children.keys()))
    assert backend_id_pre.startswith("terminal_backend_")

    del k1
    k2 = Core(Kernel(), argv=[])
    # Same ids, same parent-child link.
    assert webapp_id in k2.ctx.agents
    assert backend_id_pre in k2.ctx.agents
    web_post = k2.ctx.agents[webapp_id]
    assert backend_id_pre in web_post._children
    # terminal_webapp's _boot is idempotent — re-firing it on the live
    # rehydrated agent does NOT create a second backend.
    pre_count = len(web_post._children)
    await k2.send(webapp_id, {"type": "boot"})
    assert len(web_post._children) == pre_count


async def test_persistence_survives_corrupted_sibling(tmp_path, monkeypatch):
    """A corrupted agent.json under `agents/X/` is silently skipped;
    other agents in the same dir hydrate fine."""
    monkeypatch.chdir(tmp_path)
    k1 = Core(Kernel(), argv=[])
    rec = await k1.send(k1.id, {"type": "create_agent", "handler_module": "file.tools"})
    good_id = rec["id"]
    # Plant a corrupt sibling.
    bad = tmp_path / ".fantastic" / "agents" / "broken"
    bad.mkdir()
    (bad / "agent.json").write_text("{NOT_JSON")
    del k1
    k2 = Core(Kernel(), argv=[])
    assert good_id in k2.ctx.agents
    assert "broken" not in k2.ctx.agents


async def test_reboot_resets_in_flight_counters(tmp_path, monkeypatch):
    """Process-memory state (in_flight, inboxes, _watcher_ids) does
    NOT persist. Only records do."""
    monkeypatch.chdir(tmp_path)
    k1 = Core(Kernel(), argv=[])
    rec = await k1.send(k1.id, {"type": "create_agent", "handler_module": "file.tools"})
    aid = rec["id"]
    # Bump in_flight artificially then reboot.
    k1.ctx.agents[aid]._in_flight = 5
    del k1
    k2 = Core(Kernel(), argv=[])
    assert k2.ctx.agents[aid]._in_flight == 0


def test_agent_record_roundtrip_on_disk(tmp_path, monkeypatch):
    """The on-disk record carries id + handler_module + parent_id +
    arbitrary meta. The exact JSON shape is the contract."""
    monkeypatch.chdir(tmp_path)
    k = Core(Kernel(), argv=[])
    asyncio.get_event_loop_policy().get_event_loop()
    k.create("file.tools", id="ondisk", x=1, y=2, display_name="d")
    af = tmp_path / ".fantastic" / "agents" / "ondisk" / "agent.json"
    rec = json.loads(af.read_text())
    assert rec["id"] == "ondisk"
    assert rec["handler_module"] == "file.tools"
    assert rec["parent_id"] == "core"
    assert rec["x"] == 1 and rec["y"] == 2
    assert rec["display_name"] == "d"
