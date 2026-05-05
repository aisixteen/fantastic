"""Kernel class — storage CRUD + reflect."""

from __future__ import annotations

import json


from kernel import Kernel, INBOX_BOUND


def test_create_assigns_id_with_bundle_prefix(kernel):
    rec = kernel.create("core.tools")
    assert rec["id"].startswith("core_")
    assert rec["handler_module"] == "core.tools"


def test_create_persists_to_disk(kernel, tmp_path):
    rec = kernel.create("core.tools", model="gemma4")
    f = tmp_path / ".fantastic" / "agents" / rec["id"] / "agent.json"
    assert f.exists()
    on_disk = json.loads(f.read_text())
    assert on_disk["id"] == rec["id"]
    assert on_disk["model"] == "gemma4"


def test_create_rejects_existing_id(kernel):
    kernel.create("core.tools", id="dup")
    r = kernel.create("core.tools", id="dup")
    assert "error" in r


def test_ensure_idempotent(kernel):
    a = kernel.ensure("singleton", "core.tools", display_name="x")
    b = kernel.ensure("singleton", "core.tools", display_name="y")
    assert a["id"] == b["id"]
    # ensure does NOT overwrite existing
    assert b["display_name"] == "x"


def test_get_returns_dict_or_none(kernel):
    kernel.create("core.tools", id="agent1")
    assert kernel.get("agent1")["id"] == "agent1"
    assert kernel.get("missing") is None


def test_update_merges_meta(kernel):
    kernel.create("core.tools", id="a")
    rec = kernel.update("a", model="x", endpoint="http://localhost")
    assert rec["model"] == "x"
    assert rec["endpoint"] == "http://localhost"


def test_update_returns_none_for_missing(kernel):
    assert kernel.update("missing", x=1) is None


def test_delete_removes(kernel, tmp_path):
    kernel.create("core.tools", id="del_me")
    assert kernel.delete("del_me") is True
    assert kernel.get("del_me") is None
    assert not (tmp_path / ".fantastic" / "agents" / "del_me").exists()


def test_delete_refuses_singleton(kernel):
    kernel.ensure("locked", "core.tools", singleton=True)
    assert kernel.delete("locked") is False
    assert kernel.get("locked") is not None


def test_list_returns_all_records(kernel):
    kernel.create("core.tools", id="a")
    kernel.create("core.tools", id="b")
    ids = {a["id"] for a in kernel.list()}
    assert ids == {"a", "b"}


def test_load_all_reads_existing_agents(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    k1 = Kernel()
    k1.create("core.tools", id="persisted", x=42)
    # New kernel reads existing state
    k2 = Kernel()
    rec = k2.get("persisted")
    assert rec is not None
    assert rec["x"] == 42


def test_load_all_skips_corrupted_agent_json(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    bad_dir = tmp_path / ".fantastic" / "agents" / "broken"
    bad_dir.mkdir(parents=True)
    (bad_dir / "agent.json").write_text("{NOT_JSON")
    # Should not raise
    k = Kernel()
    assert k.get("broken") is None


async def test_reflect_kernel_returns_substrate_primer(seeded_kernel):
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert "primitive" in r
    assert "transports" in r
    assert "in_prompt" in r["transports"]
    assert "well_known" in r
    assert "core" in r["well_known"]
    assert "cli" in r["well_known"]


async def test_inbox_bounded_drops_oldest(kernel):
    # Fill an inbox past INBOX_BOUND
    target = "x"
    kernel.create("core.tools", id=target)
    for i in range(INBOX_BOUND + 50):
        await kernel.emit(target, {"type": "spam", "n": i})
    q = kernel._ensure_inbox(target)
    # Queue size should not exceed bound; oldest dropped first
    assert q.qsize() <= INBOX_BOUND
    # Newest message must be present (drained from front)
    last = None
    while not q.empty():
        last = q.get_nowait()
    assert last["n"] == INBOX_BOUND + 49
