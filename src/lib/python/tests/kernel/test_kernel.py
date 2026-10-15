"""Agent class — storage CRUD + persistence + reflect.

These tests cover the substrate's storage surface on `Agent`. Each
test gets a fresh root via the `kernel` fixture (which returns the
root Agent).
"""

from __future__ import annotations

import yaml
import json

from _testkit import boot_root, persist
from kernel import INBOX_BOUND


def test_create_assigns_id_with_bundle_prefix(kernel):
    rec = kernel.create("file_bridge.tools", allow=["."])
    assert rec["id"].startswith("file_")
    assert rec["handler_module"] == "file_bridge.tools"


def test_create_persists_to_disk(kernel, tmp_path):
    rec = kernel.create("file_bridge.tools", allow=["."], model="gemma4")
    # Agent itself never writes; the loader does. persist() is a
    # synchronous full flush (mirrors the live debounced flush).
    persist(kernel)
    f = tmp_path / ".fantastic" / "agents" / rec["id"] / "agent.yaml"
    assert f.exists()
    on_disk = yaml.safe_load(f.read_text())
    assert on_disk["id"] == rec["id"]
    assert on_disk["model"] == "gemma4"


def test_create_rejects_existing_id(kernel):
    kernel.create("file_bridge.tools", allow=["."], id="dup")
    r = kernel.create("file_bridge.tools", allow=["."], id="dup")
    assert "error" in r


def test_ensure_idempotent(kernel):
    a = kernel.ensure("singleton", "file_bridge.tools", display_name="x")
    b = kernel.ensure("singleton", "file_bridge.tools", display_name="y")
    assert a["id"] == b["id"]
    # ensure does NOT overwrite existing meta.
    assert b["display_name"] == "x"


def test_get_returns_record_or_none(kernel):
    kernel.create("file_bridge.tools", allow=["."], id="agent1")
    assert kernel.get("agent1")["id"] == "agent1"
    assert kernel.get("missing") is None


def test_update_merges_meta(kernel):
    kernel.create("file_bridge.tools", allow=["."], id="a")
    rec = kernel.update("a", model="x", endpoint="http://localhost")
    assert rec["model"] == "x"
    assert rec["endpoint"] == "http://localhost"


def test_update_returns_none_for_missing(kernel):
    assert kernel.update("missing", x=1) is None


async def test_delete_removes(kernel):
    """Cascade-delete detaches the record from the live tree. Disk
    cleanup (rmtree) is the loader's job via the `removed` event —
    covered in the kernel_state tests."""
    kernel.create("file_bridge.tools", allow=["."], id="del_me")
    r = await kernel.delete("del_me")
    assert r["deleted"] is True
    assert kernel.get("del_me") is None
    assert "del_me" not in kernel.ctx.agents


async def test_delete_refuses_locked(kernel):
    """`delete_lock` on a record refuses delete (any agent can
    self-protect via update_agent)."""
    kernel.create("file_bridge.tools", allow=["."], id="locked", delete_lock=True)
    r = await kernel.delete("locked")
    assert r.get("reason") == "locked"
    assert kernel.get("locked") is not None


def test_list_returns_all_records(kernel):
    """`agent.list()` returns flat all records across the whole tree —
    own id, all descendants."""
    kernel.create("file_bridge.tools", allow=["."], id="a")
    kernel.create("file_bridge.tools", allow=["."], id="b")
    ids = {a["id"] for a in kernel.list()}
    # Root is id "kernel_state" — included in flat list along with its children.
    assert {"a", "b"}.issubset(ids)
    assert "kernel_state" in ids


def test_load_all_reads_existing_agents(tmp_path, monkeypatch):
    """Reboot: bootstrap a fresh root in the same dir; agents the loader
    persisted from the previous root rehydrate via `read_tree` +
    `kernel.load` recursively. Same ids, same parent-child links."""
    monkeypatch.chdir(tmp_path)
    k1 = boot_root()
    k1.create("file_bridge.tools", allow=["."], id="persisted", x=42)
    persist(k1)
    # Simulate restart — drop k1, bootstrap again from same dir.
    del k1
    k2 = boot_root()
    rec = k2.get("persisted")
    assert rec is not None
    assert rec["x"] == 42


def test_load_all_skips_corrupted_agent_json(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    # Seed a real tree first, then corrupt a sibling under agents/.
    k1 = boot_root()
    k1.create("file_bridge.tools", allow=["."], id="good")
    persist(k1)
    del k1
    bad_dir = tmp_path / ".fantastic" / "agents" / "broken"
    bad_dir.mkdir(parents=True)
    (bad_dir / "agent.yaml").write_text("{NOT_JSON")
    # Should not raise; the corrupt sibling is skipped, the rest loads.
    k = boot_root()
    assert k.get("broken") is None
    assert k.get("good") is not None


def test_load_all_weak_loads_unknown_handler_module(tmp_path, monkeypatch, capsys):
    """Weak loading: an agent.yaml with a handler_module that doesn't
    import (bundle not installed in this runtime, third-party plugin
    we don't have, etc.) gets skipped + logged on boot. The record is
    left untouched on disk, the rest of the tree still loads, and
    nothing crashes. Install the bundle and the agent rehydrates on
    the next boot. Wipe-and-rebuild safe.

    Log line shape is part of the contract (grep-able from tests +
    selftests):
        [kernel] skipping agent <id>: bundle <module> not installed in this runtime
    """
    monkeypatch.chdir(tmp_path)
    # Seed a real agent + persist, then plant a ghost: a handler_module
    # pointing at a bundle not installed here.
    k = boot_root()
    k.create("file_bridge.tools", allow=["."], id="real_agent", x=42)
    persist(k)
    del k
    ghost = tmp_path / ".fantastic" / "agents" / "ghost_42"
    ghost.mkdir(parents=True)
    (ghost / "agent.yaml").write_text(
        json.dumps(
            {
                "id": "ghost_42",
                "handler_module": "ghost_bundle_that_does_not_exist.tools",
                "parent_id": "kernel_state",
            }
        )
    )

    # Restart from the same dir.
    k2 = boot_root()

    # Ghost was skipped — not in the agent map.
    assert k2.get("ghost_42") is None
    # Real agent still loads.
    real = k2.get("real_agent")
    assert real is not None and real["x"] == 42

    # Exact log line shape (grep-able across runtimes).
    err = capsys.readouterr().err
    assert (
        "[kernel] skipping agent ghost_42: bundle "
        "ghost_bundle_that_does_not_exist.tools not installed in this runtime"
    ) in err


async def test_reflect_kernel_returns_uniform_identity(seeded_kernel):
    """`send("kernel", {reflect})` returns the root's uniform identity +
    tree (default all). The reserved address "kernel" and the real id "kernel_state" give
    the same reply."""
    r = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert r["id"] == "kernel_state"
    assert r["sentence"].startswith("Fantastic kernel")
    assert r["tree"]["id"] == "kernel_state"
    assert (
        r
        == (
            await seeded_kernel.send(
                [{"target": "kernel_state", "payload": {"type": "reflect"}}]
            )
        )[0]["result"]
    )


async def test_inbox_bounded_drops_oldest(kernel):
    """Inbox queue is bounded by INBOX_BOUND and drops oldest on
    overflow (so a slow consumer doesn't block the whole substrate)."""
    target = "x"
    kernel.create("file_bridge.tools", allow=["."], id=target)
    q = kernel.ctx.ensure_inbox(target)  # the test is the reader, and registers first
    for i in range(INBOX_BOUND + 50):
        await kernel.emit(target, {"type": "spam", "n": i})
    assert q.qsize() <= INBOX_BOUND
    last = None
    while not q.empty():
        last = q.get_nowait()
    assert last["n"] == INBOX_BOUND + 49
