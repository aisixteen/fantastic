"""Agent class — storage CRUD + persistence + reflect.

These tests cover the substrate's storage surface on `Agent`. Each
test gets a fresh root via the `kernel` fixture (which returns the
root Agent).
"""

from __future__ import annotations

import json

from core import Core
from kernel import INBOX_BOUND, Kernel


def test_create_assigns_id_with_bundle_prefix(kernel):
    rec = kernel.create("file.tools")
    assert rec["id"].startswith("file_")
    assert rec["handler_module"] == "file.tools"


def test_create_persists_to_disk(kernel, tmp_path):
    rec = kernel.create("file.tools", model="gemma4")
    f = tmp_path / ".fantastic" / "agents" / rec["id"] / "agent.json"
    assert f.exists()
    on_disk = json.loads(f.read_text())
    assert on_disk["id"] == rec["id"]
    assert on_disk["model"] == "gemma4"


def test_create_rejects_existing_id(kernel):
    kernel.create("file.tools", id="dup")
    r = kernel.create("file.tools", id="dup")
    assert "error" in r


def test_ensure_idempotent(kernel):
    a = kernel.ensure("singleton", "file.tools", display_name="x")
    b = kernel.ensure("singleton", "file.tools", display_name="y")
    assert a["id"] == b["id"]
    # ensure does NOT overwrite existing meta.
    assert b["display_name"] == "x"


def test_get_returns_record_or_none(kernel):
    kernel.create("file.tools", id="agent1")
    assert kernel.get("agent1")["id"] == "agent1"
    assert kernel.get("missing") is None


def test_update_merges_meta(kernel):
    kernel.create("file.tools", id="a")
    rec = kernel.update("a", model="x", endpoint="http://localhost")
    assert rec["model"] == "x"
    assert rec["endpoint"] == "http://localhost"


def test_update_returns_none_for_missing(kernel):
    assert kernel.update("missing", x=1) is None


async def test_delete_removes(kernel, tmp_path):
    kernel.create("file.tools", id="del_me")
    r = await kernel.delete("del_me")
    assert r["deleted"] is True
    assert kernel.get("del_me") is None
    assert not (tmp_path / ".fantastic" / "agents" / "del_me").exists()


async def test_delete_refuses_locked(kernel):
    """`delete_lock` on a record refuses delete (any agent can
    self-protect via update_agent)."""
    kernel.create("file.tools", id="locked", delete_lock=True)
    r = await kernel.delete("locked")
    assert r.get("locked") is True
    assert kernel.get("locked") is not None


def test_list_returns_all_records(kernel):
    """`agent.list()` returns flat all records across the whole tree —
    own id, all descendants."""
    kernel.create("file.tools", id="a")
    kernel.create("file.tools", id="b")
    ids = {a["id"] for a in kernel.list()}
    # Root is id "core" — included in flat list along with its children.
    assert {"a", "b"}.issubset(ids)
    assert "core" in ids


def test_load_all_reads_existing_agents(tmp_path, monkeypatch):
    """Reboot: bootstrap a fresh root in the same dir; agents that
    were persisted from the previous root rehydrate via _load_children
    recursively. Same ids, same parent-child links."""
    monkeypatch.chdir(tmp_path)
    k1 = Core(Kernel(), argv=[])
    k1.create("file.tools", id="persisted", x=42)
    # Simulate restart — drop k1, bootstrap again from same dir.
    del k1
    k2 = Core(Kernel(), argv=[])
    rec = k2.get("persisted")
    assert rec is not None
    assert rec["x"] == 42


def test_load_all_skips_corrupted_agent_json(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    # Corrupted file under root's agents/ dir.
    bad_dir = tmp_path / ".fantastic" / "agents" / "broken"
    bad_dir.mkdir(parents=True)
    (bad_dir / "agent.json").write_text("{NOT_JSON")
    # Should not raise.
    k = Core(Kernel(), argv=[])
    assert k.get("broken") is None


async def test_reflect_kernel_returns_substrate_primer(seeded_kernel):
    """`send("kernel", {reflect})` returns root's primer (transports +
    tree + bundles). Whether you use the magic id "kernel" or root's
    actual id "core", you get the primer."""
    r = await seeded_kernel.send("kernel", {"type": "reflect"})
    assert "primitive" in r
    assert "transports" in r
    assert "in_prompt" in r["transports"]
    assert "tree" in r
    # cli was seeded as a child of root (well-known singleton).
    assert "available_bundles" in r


async def test_inbox_bounded_drops_oldest(kernel):
    """Inbox queue is bounded by INBOX_BOUND and drops oldest on
    overflow (so a slow consumer doesn't block the whole substrate)."""
    target = "x"
    kernel.create("file.tools", id=target)
    for i in range(INBOX_BOUND + 50):
        await kernel.emit(target, {"type": "spam", "n": i})
    q = kernel.ctx.inboxes[target]
    assert q.qsize() <= INBOX_BOUND
    last = None
    while not q.empty():
        last = q.get_nowait()
    assert last["n"] == INBOX_BOUND + 49
