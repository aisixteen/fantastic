"""kernel_state — pure disk read/write/forget + round-trip through Kernel.load,
plus the live subscribe→flush→rmtree wiring of the booted root loader."""

import asyncio
import json
from pathlib import Path

from _testkit import boot_root
from kernel_state.tools import (
    _find_store,
    on_shutdown,
    read_tree,
    rmtree,
    write_record,
)
from kernel import Kernel


def _stage(tmp_path, monkeypatch):
    """chdir into tmp_path and stage a nested .fantastic tree under cwd (the disk
    surface clamps to cwd, so the staged root must be cwd-relative). Returns the
    cwd-relative root dir. (Staging writes directly — that's test setup; the code
    under test, read_tree, goes through `fs`.)"""
    monkeypatch.chdir(tmp_path)
    root = Path(".fantastic")
    (root / "agents" / "web" / "agents" / "web_ws").mkdir(parents=True)
    (root / "agent.json").write_text(
        json.dumps({"id": "kernel_state", "handler_module": "kernel_state.tools"})
    )
    (root / "agents" / "web" / "agent.json").write_text(
        json.dumps(
            {
                "id": "web",
                "handler_module": "web.tools",
                "parent_id": "kernel_state",
                "port": 8888,
            }
        )
    )
    (root / "agents" / "web" / "agents" / "web_ws" / "agent.json").write_text(
        json.dumps(
            {"id": "web_ws", "handler_module": "web_ws.tools", "parent_id": "web"}
        )
    )
    return root


def test_read_tree_flat_with_parent_ids(tmp_path, monkeypatch):
    root = _stage(tmp_path, monkeypatch)
    records = read_tree(root)
    by_id = {r["id"]: r for r in records}
    assert set(by_id) == {"kernel_state", "web", "web_ws"}
    assert by_id["kernel_state"].get("parent_id") in (None,)  # root
    assert by_id["web"]["parent_id"] == "kernel_state"
    assert by_id["web_ws"]["parent_id"] == "web"
    assert by_id["web"]["port"] == 8888


def test_read_tree_feeds_kernel_load(tmp_path, monkeypatch):
    root = _stage(tmp_path, monkeypatch)
    k = Kernel()
    k.load(read_tree(root), root_path=root)
    assert set(k.agents) == {"kernel_state", "web", "web_ws"}
    assert k.root.id == "kernel_state"
    # derived addresses mirror the nested disk layout
    assert (
        k.get_agent("web_ws")._root_path
        == root / "agents" / "web" / "agents" / "web_ws"
    )


def test_write_record_merge_not_overwrite(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    d = Path("a")
    # a sidecar field a bundle wrote, plus a kernel field
    write_record(d, {"id": "a", "handler_module": "x.tools", "custom_sidecar": 1})
    # the kernel re-persists only its keys; the sidecar field must survive
    write_record(d, {"id": "a", "handler_module": "x.tools", "x": 2})
    on_disk = json.loads((d / "agent.json").read_text())
    assert on_disk["custom_sidecar"] == 1  # preserved
    assert on_disk["x"] == 2  # added
    assert on_disk["id"] == "a"


def test_rmtree_removes_dir(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    d = Path("gone")
    (d / "agents" / "child").mkdir(parents=True)
    (d / "agent.json").write_text("{}")
    rmtree(d)
    assert not d.exists()


async def test_no_provider_state_stays_in_ram(tmp_path, monkeypatch):
    """NO FALLBACK (kernel-style): with no persistence provider wired, the booted
    loader does NOT write records to disk — state lives in RAM only. The loader
    never papers over a missing provider with a direct-write back-channel."""
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    await root.send(root.id, {"type": "boot"})  # start the flush loop (no provider)
    try:
        rec = await root.send(
            root.id,
            {
                "type": "create_agent",
                "handler_module": "yaml_state.tools",
                "mode": "mem",
            },
        )
        aid = rec["id"]
        await asyncio.sleep(0.3)  # debounce window
        assert root.ctx.get_agent(aid) is not None  # live in the tree (RAM)
        assert not (tmp_path / ".fantastic" / "agents" / aid / "agent.json").exists()
    finally:
        await on_shutdown(root)


async def test_booted_loader_persists_via_discovered_provider(tmp_path, monkeypatch):
    """The stream-consumer path: an operator/LLM creates a PERSISTENT, gated
    file_bridge child rooted at `.fantastic`; the booted loader DISCOVERS it (by
    match, not a fixed id) and persists records THROUGH its `write_stream` — not
    direct disk I/O. The record bytes + the seeded readme reach disk via the SINK,
    and delete recurses through the provider."""
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    # operator/LLM wires the provider: a real, persisted, gated file_bridge child.
    root.create(
        "file_bridge.tools", id="store", root=".fantastic", ingress_rule="allow_all"
    )
    assert (
        _find_store(root) == "store"
    )  # discovered by match (root resolves to .fantastic)
    await root.send(root.id, {"type": "boot"})  # start the flush loop
    try:
        rec = await root.send(
            root.id,
            {"type": "create_agent", "handler_module": "file_bridge.tools", "x": 7},
        )
        aid = rec["id"]
        agent_dir = tmp_path / ".fantastic" / "agents" / aid
        await asyncio.sleep(0.3)  # let the debounce flush stream the record
        assert (agent_dir / "agent.json").exists()
        assert json.loads((agent_dir / "agent.json").read_text())["x"] == 7
        assert (agent_dir / "readme.md").exists()  # readme seeded via stream too

        await root.send(root.id, {"type": "delete_agent", "id": aid})
        await asyncio.sleep(0.3)  # recursive delete routed through the provider
        assert not agent_dir.exists()
    finally:
        await on_shutdown(root)


async def test_session_loader_serves_sub_namespace(tmp_path, monkeypatch):
    """A SESSION loader (a `root` meta + `watch=false`) serves a federated JS
    kernel's records under `.fantastic/web/<session>/`, separate from the host
    tree. load_tree / persist_record / forget_record operate under `root`.
    This is the host side of the proxy_loader."""
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    sess = ".fantastic/web/s1"
    rec = await root.send(
        root.id,
        {
            "type": "create_agent",
            "handler_module": "kernel_state.tools",
            "watch": False,
            "root": sess,
        },
    )
    sid = rec["id"]
    # boot seeds the namespace anchor (read_tree needs a root agent.json)
    assert (tmp_path / sess / "agent.json").exists()

    # persist two JS view records (NOT live host agents) — they nest under
    # the session root. The JS root (canvas) sends parent_id = the session
    # loader's id, so it lands as a direct child of the namespace.
    await root.send(
        sid,
        {
            "type": "persist_record",
            "record": {
                "id": "canvas",
                "handler_module": "canvas.ts",
                "parent_id": sid,
            },
        },
    )
    await root.send(
        sid,
        {
            "type": "persist_record",
            "record": {
                "id": "term1",
                "handler_module": "terminal_view.ts",
                "parent_id": "canvas",
            },
        },
    )
    canvas_dir = tmp_path / sess / "agents" / "canvas"
    term_dir = canvas_dir / "agents" / "term1"
    assert (canvas_dir / "agent.json").exists()
    assert (term_dir / "agent.json").exists()

    # load_tree reads the namespace back (anchor + canvas + term1)
    reply = await root.send(sid, {"type": "load_tree"})
    by_id = {r["id"]: r for r in reply["records"]}
    assert {sid, "canvas", "term1"}.issubset(by_id)
    assert by_id["canvas"]["parent_id"] == sid  # disk position
    assert by_id["term1"]["parent_id"] == "canvas"

    # forget removes the JS agent's nested dir (parent_id resolves the path)
    await root.send(
        sid, {"type": "forget_record", "id": "term1", "parent_id": "canvas"}
    )
    assert not term_dir.exists()
    assert canvas_dir.exists()  # sibling untouched
