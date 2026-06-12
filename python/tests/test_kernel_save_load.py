"""Kernel.save/load — the medium-agnostic snapshot round-trip (mirror Rust)."""

import pytest

from kernel import Agent, Kernel  # noqa: F401  (Agent used indirectly via load)
from kernel._state import CURRENT_VERSION, SnapshotError, validate_records


def test_load_builds_tree_in_memory():
    k = Kernel()
    k.load(
        [
            {"id": "kernel_state"},
            {
                "id": "web",
                "handler_module": "web.tools",
                "parent_id": "kernel_state",
                "port": 8888,
            },
            {
                "id": "f",
                "handler_module": "file_bridge.tools",
                "parent_id": "kernel_state",
            },
        ]
    )
    assert set(k.agents) == {"kernel_state", "web", "f"}
    assert k.root is not None and k.root.id == "kernel_state"
    assert k.get("web")["port"] == 8888
    assert k.get("web")["parent_id"] == "kernel_state"


def test_save_load_roundtrip():
    k = Kernel()
    k.load(
        [
            {"id": "kernel_state"},
            {
                "id": "web",
                "handler_module": "web.tools",
                "parent_id": "kernel_state",
                "port": 9,
            },
            {"id": "f", "handler_module": "file_bridge.tools", "parent_id": "web"},
        ]
    )
    snap = k.save()
    assert snap["version"] == CURRENT_VERSION
    assert {r["id"] for r in snap["records"]} == {"kernel_state", "web", "f"}
    # deterministic id-sorted order
    assert [r["id"] for r in snap["records"]] == ["f", "kernel_state", "web"]

    k2 = Kernel()
    k2.load(snap)  # accepts the {version, records} envelope
    assert set(k2.agents) == {"kernel_state", "web", "f"}
    assert k2.get("f")["parent_id"] == "web"


def test_weak_load_skips_unknown_bundle_and_subtree():
    k = Kernel()
    k.load(
        [
            {"id": "kernel_state"},
            {
                "id": "x",
                "handler_module": "totally_not_a_bundle.tools",
                "parent_id": "kernel_state",
            },
            {
                "id": "y",
                "handler_module": "file_bridge.tools",
                "parent_id": "x",
            },  # child of x
            {
                "id": "z",
                "handler_module": "file_bridge.tools",
                "parent_id": "kernel_state",
            },
        ]
    )
    # x (unregistered handler) + its child y are skipped; kernel_state + z survive.
    assert set(k.agents) == {"kernel_state", "z"}


def test_save_skips_ephemeral():
    # An ephemeral agent never round-trips through a snapshot.
    class Eph(Agent):
        ephemeral = True

    k = Kernel()
    k.load([{"id": "kernel_state"}])
    Eph("eph1", ctx=k, parent=k.root)  # composed in-memory, not persisted
    assert "eph1" in k.agents
    ids = {r["id"] for r in k.save()["records"]}
    assert ids == {"kernel_state"}  # the ephemeral child is excluded


def test_validate_rejects_bad_snapshots():
    with pytest.raises(SnapshotError):
        validate_records([{"id": "a", "parent_id": "missing"}])  # no root + dangling
    with pytest.raises(SnapshotError):
        validate_records([{"id": "a"}, {"id": "a"}])  # duplicate id
    with pytest.raises(SnapshotError):
        validate_records([{"id": "a"}, {"id": "b"}])  # two roots
    with pytest.raises(SnapshotError):
        validate_records([{"id": "a"}], version=CURRENT_VERSION + 1)  # future version
