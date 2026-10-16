"""kernel_state — pure disk read/write/forget + round-trip through Kernel.load,
plus the live subscribe→flush→rmtree wiring of the booted root loader."""

import yaml
import asyncio
import json
from pathlib import Path

from file_bridge import fs
from kernel_state.tools import (
    _find_store,
    on_shutdown,
    read_tree,
    rmtree,
    write_record,
)

from _testkit import boot_root
from kernel import Kernel


def _stage(tmp_path, monkeypatch):
    """chdir into tmp_path and stage a nested .fantastic tree under cwd (the disk
    surface clamps to the running dir, so the staged root must be under it). Returns the
    cwd-relative root dir. (Staging writes directly — that's test setup; the code
    under test, read_tree, goes through `fs`.)"""
    monkeypatch.chdir(tmp_path)
    root = Path(".fantastic")
    (root / "agents" / "web" / "agents" / "web_ws").mkdir(parents=True)
    (root / "agent.yaml").write_text(
        json.dumps({"id": "kernel_state", "handler_module": "kernel_state.tools"})
    )
    (root / "agents" / "web" / "agent.yaml").write_text(
        json.dumps(
            {
                "id": "web",
                "handler_module": "web.tools",
                "parent_id": "kernel_state",
                "port": 8888,
            }
        )
    )
    (root / "agents" / "web" / "agents" / "web_ws" / "agent.yaml").write_text(
        json.dumps(
            {"id": "web_ws", "handler_module": "web_ws.tools", "parent_id": "web"}
        )
    )
    return root


def test_read_tree_flat_with_parent_ids(tmp_path, monkeypatch):
    root = _stage(tmp_path, monkeypatch)
    records = read_tree(fs.Root(tmp_path, root))
    by_id = {r["id"]: r for r in records}
    assert set(by_id) == {"kernel_state", "web", "web_ws"}
    assert by_id["kernel_state"].get("parent_id") in (None,)  # root
    assert by_id["web"]["parent_id"] == "kernel_state"
    assert by_id["web_ws"]["parent_id"] == "web"
    assert by_id["web"]["port"] == 8888


def test_read_tree_feeds_kernel_load(tmp_path, monkeypatch):
    root = _stage(tmp_path, monkeypatch)
    k = Kernel()
    k.load(read_tree(fs.Root(tmp_path, root)), root_path=root)
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
    write_record(
        fs.Root(tmp_path, d),
        {"id": "a", "handler_module": "x.tools", "custom_sidecar": 1},
    )
    # the kernel re-persists only its keys; the sidecar field must survive
    write_record(fs.Root(tmp_path, d), {"id": "a", "handler_module": "x.tools", "x": 2})
    on_disk = yaml.safe_load((d / "agent.yaml").read_text(encoding="utf-8"))
    assert on_disk["custom_sidecar"] == 1  # preserved
    assert on_disk["x"] == 2  # added
    assert on_disk["id"] == "a"


def test_rmtree_removes_dir(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    d = Path("gone")
    (d / "agents" / "child").mkdir(parents=True)
    (d / "agent.yaml").write_text("{}")
    rmtree(fs.Root(tmp_path, d))
    assert not d.exists()


async def test_no_provider_state_stays_in_ram(tmp_path, monkeypatch):
    """NO FALLBACK (kernel-style): with no persistence provider wired, the booted
    loader does NOT write records to disk — state lives in RAM only. The loader
    never papers over a missing provider with a direct-write back-channel."""
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    (await root.send([{"target": root.id, "payload": {"type": "boot"}}]))[0][
        "result"
    ]  # start the flush loop (no provider)
    try:
        rec = (
            await root.send(
                [
                    {
                        "target": root.id,
                        "payload": {
                            "type": "create_agent",
                            "parent_id": root.id,
                            "handler_module": "file_bridge.tools",
                            "mode": "mem",
                        },
                    }
                ]
            )
        )[0]["result"]
        aid = rec["id"]
        await asyncio.sleep(0.3)  # debounce window
        assert root.ctx.get_agent(aid) is not None  # live in the tree (RAM)
        assert not (tmp_path / ".fantastic" / "agents" / aid / "agent.yaml").exists()
    finally:
        await on_shutdown(root)


async def test_booted_loader_persists_via_discovered_provider(tmp_path, monkeypatch):
    """The stream-consumer path: an operator/LLM creates a PERSISTENT, gated
    file_bridge child rooted at `.fantastic`; the booted loader DISCOVERS it (by
    match, not a fixed id) and persists records THROUGH its `write_stream` — not
    direct disk I/O. The record bytes reach disk via the SINK (no readme copy),
    and delete recurses through the provider."""
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    # operator/LLM wires the provider: a real, persisted, gated file_bridge child.
    root.create(
        "file_bridge.tools",
        allow=["."],
        id="store",
        root=".fantastic",
        ingress_rule="allow_all_ingress",
    )
    assert (
        _find_store(root) == "store"
    )  # discovered by match (root resolves to .fantastic)
    (await root.send([{"target": root.id, "payload": {"type": "boot"}}]))[0][
        "result"
    ]  # start the flush loop
    try:
        rec = (
            await root.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": root.id,
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                            "x": 7,
                        },
                    }
                ]
            )
        )[0]["result"]
        aid = rec["id"]
        agent_dir = tmp_path / ".fantastic" / "agents" / aid
        await asyncio.sleep(0.3)  # let the debounce flush stream the record
        assert (agent_dir / "agent.yaml").exists()
        assert (
            yaml.safe_load((agent_dir / "agent.yaml").read_text(encoding="utf-8"))["x"]
            == 7
        )
        assert not (agent_dir / "readme.md").exists()  # served from the bundle

        (
            await root.send(
                [{"target": root.id, "payload": {"type": "delete_agent", "id": aid}}]
            )
        )[0]["result"]
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
    rec = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "handler_module": "kernel_state.tools",
                        "watch": False,
                        "root": sess,
                    },
                }
            ]
        )
    )[0]["result"]
    sid = rec["id"]
    # the namespace keeps no copy of the loader's record — it lives in the tree
    assert not (tmp_path / sess / "agent.yaml").exists()

    # persist two JS view records (NOT live host agents) — they nest under
    # the session root. The JS root (canvas) sends parent_id = the session
    # loader's id, so it lands as a direct child of the namespace.
    (
        await root.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "persist_record",
                        "record": {
                            "id": "canvas",
                            "handler_module": "canvas.ts",
                            "parent_id": sid,
                        },
                    },
                }
            ]
        )
    )[0]["result"]
    (
        await root.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "persist_record",
                        "record": {
                            "id": "term1",
                            "handler_module": "terminal_view.ts",
                            "parent_id": "canvas",
                        },
                    },
                }
            ]
        )
    )[0]["result"]
    canvas_dir = tmp_path / sess / "agents" / "canvas"
    term_dir = canvas_dir / "agents" / "term1"
    assert (canvas_dir / "agent.yaml").exists()
    assert (term_dir / "agent.yaml").exists()

    # load_tree reads the namespace back, rooted at the loader's LIVE record
    reply = (await root.send([{"target": sid, "payload": {"type": "load_tree"}}]))[0][
        "result"
    ]
    by_id = {r["id"]: r for r in reply["records"]}
    assert {sid, "canvas", "term1"}.issubset(by_id)
    assert by_id[sid] == root.ctx.agents[sid].record
    assert by_id["canvas"]["parent_id"] == sid  # disk position
    assert by_id["term1"]["parent_id"] == "canvas"

    # forget removes the JS agent's nested dir (parent_id resolves the path)
    (
        await root.send(
            [
                {
                    "target": sid,
                    "payload": {
                        "type": "forget_record",
                        "id": "term1",
                        "parent_id": "canvas",
                    },
                }
            ]
        )
    )[0]["result"]
    assert not term_dir.exists()
    assert canvas_dir.exists()  # sibling untouched


async def test_a_record_commits_atomically_and_leaves_no_debris(
    seeded_kernel, store_agent, tmp_path
):
    """A record is streamed to a tmp beside the target and RENAMED into place.

    A `write_stream` that dies after truncating would otherwise leave a
    half-written or empty `agent.yaml` — the record gone, and the next boot
    hydrating a tree with a hole in it. The commit is the leg's own single atomic
    op, so the outcomes are "the old record survives" or "the new one is
    complete", never a torn file. What this pins is the visible half: the file
    parses, and no `.tmp.` debris is left behind.
    """

    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "id": "atomic_probe",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "display_name": "Atomic",
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == "atomic_probe", rec
    # Persist explicitly rather than racing the debounced auto-flush.
    w = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {"type": "persist_record", "record": rec},
                }
            ]
        )
    )[0]["result"]
    assert not (isinstance(w, dict) and w.get("error")), w

    root = tmp_path / ".fantastic"
    agent_dir = root / "agents" / "atomic_probe"
    on_disk = agent_dir / "agent.yaml"
    assert on_disk.exists(), sorted(p.name for p in agent_dir.iterdir())
    parsed = yaml.safe_load(on_disk.read_text(encoding="utf-8"))
    assert parsed["id"] == "atomic_probe" and parsed["display_name"] == "Atomic"
    debris = [p.name for p in agent_dir.iterdir() if ".tmp." in p.name]
    assert debris == [], f"the commit must leave no tmp behind: {debris}"


# ─── read_bundles: folder bundles as TEXT, for a kernel that loads its own ──


async def _ask(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def test_read_bundles_hands_each_folders_bundle_yaml_raw(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    sess = ".fantastic/web"
    sid = (
        await _ask(
            root,
            root.id,
            {
                "type": "create_agent",
                "parent_id": root.id,
                "handler_module": "kernel_state.tools",
                "watch": False,
                "root": sess,
            },
        )
    )["id"]
    text = "base: html_agent\nsentence: A clock.\nhtml: index.html  # not parsed here\n"
    (tmp_path / sess / "bundles" / "clock").mkdir(parents=True)
    (tmp_path / sess / "bundles" / "clock" / "bundle.yaml").write_text(text)
    (tmp_path / sess / "bundles" / "empty").mkdir()
    (tmp_path / sess / "bundles" / "stray.txt").write_text("not a folder")

    got = await _ask(root, sid, {"type": "read_bundles"})
    assert got == {
        "bundles": [
            {"name": "clock", "folder": "web/bundles/clock", "text": text},
            {
                "name": "empty",
                "folder": "web/bundles/empty",
                "error": "bundles/empty: no bundle.yaml",
            },
        ]
    }
    # one by name; the loader reads ITS OWN root, never another's
    one = await _ask(root, sid, {"type": "read_bundles", "name": "clock"})
    assert one["bundles"] == got["bundles"][:1]
    assert (await _ask(root, root.id, {"type": "read_bundles"})) == {"bundles": []}


async def test_read_bundles_refuses_what_is_no_folder(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    for name, reason in (("nope", "not_found"), ("../x", "not_found"), ("", "bad_arg")):
        r = await _ask(root, root.id, {"type": "read_bundles", "name": name})
        assert r.get("reason") == reason, (name, r)
