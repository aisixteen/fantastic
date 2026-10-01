"""The ONE persistence algorithm kernel_state implements TWICE — once straight
on `fs` (`write_record` / `_seed_readme`, the cold bootstrap path) and once
THROUGH a discovered file_bridge store (`_persist_via_store`, the auto-flush) —
pinned on both sinks.

Folding them into one algorithm over a sink interface (reduction candidate F)
must keep, on EACH sink: copy-if-missing for readme.md and every tier of the
`docs/` tree (an operator edit is never clobbered, a deleted doc is re-seeded),
the merge that keeps unknown on-disk keys (a record larger than one 64 KiB
read_stream chunk included), and the commit that never leaves a torn record.
"""

from __future__ import annotations

import importlib.resources
from pathlib import Path

import pytest
import yaml

import file_bridge.tools as fbt
from file_bridge import fs
from kernel_state.tools import on_shutdown, write_record

from _testkit import boot_root

PKG = importlib.resources.files("kernel_state")
SHIPPED = {
    "readme.md": (PKG / "readme.md").read_text(encoding="utf-8"),
    "docs/memory.md": (PKG / "docs" / "memory.md").read_text(encoding="utf-8"),
    "docs/frontend/routing.md": (PKG / "docs" / "frontend" / "routing.md").read_text(
        encoding="utf-8"
    ),
    "docs/frontend/panel-recipe.md": (
        PKG / "docs" / "frontend" / "panel-recipe.md"
    ).read_text(encoding="utf-8"),
}


def _tree(d: Path) -> list[str]:
    return sorted(str(p.relative_to(d)) for p in d.rglob("*") if p.is_file())


def _shipped_tree() -> list[str]:
    out = ["readme.md"]

    def walk(src, rel):
        for c in src.iterdir():
            if c.is_dir():
                walk(c, f"{rel}/{c.name}")
            elif c.name.endswith(".md"):
                out.append(f"{rel}/{c.name}")

    walk(PKG / "docs", "docs")
    return sorted(out)


def _edit_and_delete(d: Path) -> None:
    (d / "readme.md").write_text("OPERATOR README")
    (d / "docs" / "frontend" / "routing.md").write_text("OPERATOR ROUTING")
    (d / "docs" / "memory.md").unlink()
    (d / "docs" / "frontend" / "panel-recipe.md").unlink()


def _assert_copy_if_missing(d: Path) -> None:
    assert (d / "readme.md").read_text() == "OPERATOR README"
    assert (d / "docs" / "frontend" / "routing.md").read_text() == "OPERATOR ROUTING"
    # the deleted ones — a top tier and a nested tier — are re-seeded verbatim
    assert (d / "docs" / "memory.md").read_text() == SHIPPED["docs/memory.md"]
    assert (d / "docs" / "frontend" / "panel-recipe.md").read_text() == SHIPPED[
        "docs/frontend/panel-recipe.md"
    ]


# ─── the direct sink (write_record) ──────────────────────────────────────────


def test_direct_sink_seeds_every_tier_then_never_clobbers(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    d = tmp_path / "rec"
    rec = {"id": "rec", "handler_module": "kernel_state.tools"}
    write_record(fs.Root(tmp_path, Path("rec")), rec)
    assert _tree(d) == sorted(["agent.yaml", *_shipped_tree()])
    assert (d / "docs" / "frontend" / "routing.md").read_text() == SHIPPED[
        "docs/frontend/routing.md"
    ]
    _edit_and_delete(d)
    write_record(fs.Root(tmp_path, Path("rec")), {**rec, "x": 1})
    _assert_copy_if_missing(d)


def test_direct_sink_merge_keeps_unknown_keys_and_key_order(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    d = tmp_path / "m"
    d.mkdir()
    (d / "agent.yaml").write_text("id: m\nunknown: kept\nx: 1\n")
    write_record(fs.Root(tmp_path, Path("m")), {"id": "m", "x": 2, "y": 3})
    on_disk = yaml.safe_load((d / "agent.yaml").read_text())
    assert on_disk == {"id": "m", "unknown": "kept", "x": 2, "y": 3}
    assert list(on_disk) == ["id", "unknown", "x", "y"]


def test_direct_sink_without_a_package_seeds_nothing(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    for hm in (None, "no_such_pkg_pin.tools", "kernel"):
        write_record(fs.Root(tmp_path, Path("n")), {"id": "n", "handler_module": hm})
    assert _tree(tmp_path / "n") == ["agent.yaml"]


# ─── the store sink (the auto-flush through a discovered file_bridge) ────────


@pytest.fixture
async def wired(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    root.create(
        "file_bridge.tools",
        allow=["."],
        id="store",
        root=".fantastic",
        ingress_rule="allow_all_ingress",
    )
    (await root.send([{"target": root.id, "payload": {"type": "boot"}}]))[0]
    try:
        yield root
    finally:
        await on_shutdown(root)


async def _flush(root) -> None:
    await root._fs_flush_loop.aflush()


async def test_store_sink_seeds_every_tier_then_never_clobbers(wired, tmp_path):
    root = wired
    d = tmp_path / ".fantastic"
    # the bootstrap seeded the root's docs straight on disk; edit + delete some
    _edit_and_delete(d)
    root.ctx.update(root.id, note="touch")  # an `updated` event → persist via store
    await _flush(root)
    _assert_copy_if_missing(d)
    assert yaml.safe_load((d / "agent.yaml").read_text())["note"] == "touch"


async def test_store_sink_seeds_a_new_agents_whole_doc_tree(wired, tmp_path):
    root = wired
    root.create("kernel_state.tools", id="session", watch=False, root="sess")
    await _flush(root)
    d = tmp_path / ".fantastic" / "agents" / "session"
    assert _tree(d) == sorted(["agent.yaml", *_shipped_tree()])
    _edit_and_delete(d)
    root.ctx.update("session", note=1)
    await _flush(root)
    _assert_copy_if_missing(d)


async def test_store_sink_merge_keeps_unknown_keys_past_one_chunk(wired, tmp_path):
    root = wired
    root.create("file_bridge.tools", id="big", allow=["."])
    await _flush(root)
    f = tmp_path / ".fantastic" / "agents" / "big" / "agent.yaml"
    on_disk = yaml.safe_load(f.read_text())
    # an operator-added key big enough that the record spans two read_stream chunks,
    # and one more unknown key AFTER it (only a paged read reaches it)
    on_disk["blob"] = "x" * 70_000
    on_disk["tail_key"] = "kept"
    f.write_text(yaml.safe_dump(on_disk, sort_keys=False))
    assert f.stat().st_size > 65_536
    root.ctx.update("big", y=2)
    await _flush(root)
    after = yaml.safe_load(f.read_text())
    assert after["blob"] == "x" * 70_000 and after["tail_key"] == "kept"
    assert after["y"] == 2 and after["id"] == "big"


async def test_store_sink_a_corrupt_record_is_replaced_not_merged(wired, tmp_path):
    root = wired
    root.create("file_bridge.tools", id="torn", allow=["."])
    await _flush(root)
    f = tmp_path / ".fantastic" / "agents" / "torn" / "agent.yaml"
    f.write_text("{not: yaml: at all")
    root.ctx.update("torn", y=5)
    await _flush(root)
    after = yaml.safe_load(f.read_text())
    assert after["y"] == 5 and after["id"] == "torn"


async def test_store_sink_a_refused_write_leaves_the_old_record(
    wired, tmp_path, capsys
):
    root = wired
    root.create("file_bridge.tools", id="keep", allow=["."], v=1)
    await _flush(root)
    f = tmp_path / ".fantastic" / "agents" / "keep" / "agent.yaml"
    before = f.read_text()
    root.ctx.update("store", readonly=True)
    await _flush(root)  # the store's own update is refused too
    capsys.readouterr()
    root.ctx.update("keep", v=2)
    await _flush(root)
    assert f.read_text() == before
    err = capsys.readouterr().err
    assert (
        "  [kernel_state] persist through 'store' failed: agents/keep/agent.yaml" in err
    )
    r = (await root.send([{"target": "kernel", "payload": {"type": "reflect"}}]))[0][
        "result"
    ]
    assert r["persistence"]["provider"] is None
    assert r["persistence"]["error"]["path"] == "agents/keep/agent.yaml"


async def test_store_sink_a_failed_rename_leaves_the_old_record_and_the_tmp(
    wired, tmp_path, monkeypatch, capsys
):
    root = wired
    root.create("file_bridge.tools", id="mv", allow=["."], v=1)
    await _flush(root)
    d = tmp_path / ".fantastic" / "agents" / "mv"
    before = (d / "agent.yaml").read_text()

    async def refuse(id, payload, kernel):
        return {"error": "rename: disk full", "reason": "bad_arg"}

    monkeypatch.setitem(fbt.VERBS, "rename", refuse)
    root.ctx.update("mv", v=2)
    await _flush(root)
    assert (d / "agent.yaml").read_text() == before
    debris = [p.name for p in d.iterdir() if p.name.startswith("agent.yaml.tmp.")]
    assert len(debris) == 1, debris
    assert yaml.safe_load((d / debris[0]).read_text())["v"] == 2
    assert (
        "  [kernel_state] persist through 'store' failed: agents/mv/agent.yaml: "
        "rename: disk full"
    ) in capsys.readouterr().err


async def test_store_sink_forget_removes_the_whole_subtree(wired, tmp_path):
    root = wired
    root.create("file_bridge.tools", id="parent", allow=["."])
    root.ctx.agents["parent"].create("file_bridge.tools", id="child", allow=["."])
    await _flush(root)
    d = tmp_path / ".fantastic" / "agents" / "parent"
    assert (d / "agents" / "child" / "agent.yaml").exists()
    out = (
        await root.send(
            [{"target": "kernel", "payload": {"type": "delete_agent", "id": "parent"}}]
        )
    )[0]
    assert out["status"] == "completed", out
    await _flush(root)
    assert not d.exists()
    assert (tmp_path / ".fantastic" / "agent.yaml").exists()  # the root stays


# ─── the record verbs on the direct sink (persist_record / forget_record) ────


async def _root_call(root, payload) -> dict:
    return (await root.send([{"target": root.id, "payload": payload}]))[0]["result"]


async def test_record_verbs_refuse_a_missing_or_unsafe_id(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    for rec in (None, {}, {"id": ""}, "x"):
        assert await _root_call(root, {"type": "persist_record", "record": rec}) == {
            "error": "persist_record: record (dict with id) required",
            "reason": "missing_arg",
            "arg": "record (dict with id)",
        }, rec
    assert await _root_call(root, {"type": "forget_record"}) == {
        "error": "forget_record: id required",
        "reason": "missing_arg",
        "arg": "id",
    }
    r = await _root_call(root, {"type": "persist_record", "record": {"id": "../x"}})
    assert r == {
        "error": "persist_record: id must be a single path segment (no separators); "
        "got '../x'",
        "reason": "bad_arg",
        "arg": "id",
    }
    r = await _root_call(root, {"type": "forget_record", "id": ".."})
    assert r == {
        "error": "forget_record: id must be a name that is not a path traversal; "
        "got '..'",
        "reason": "bad_arg",
        "arg": "id",
    }


async def test_forget_record_removes_a_live_agents_subtree_only(tmp_path, monkeypatch):
    from _testkit import persist

    monkeypatch.chdir(tmp_path)
    root = boot_root()
    root.create("file_bridge.tools", id="gone", allow=["."])
    root.ctx.agents["gone"].create("file_bridge.tools", id="gone_kid", allow=["."])
    root.create("file_bridge.tools", id="kept", allow=["."])
    persist(root)
    agents = tmp_path / ".fantastic" / "agents"
    assert (agents / "gone" / "agents" / "gone_kid" / "agent.yaml").exists()
    assert await _root_call(root, {"type": "forget_record", "id": "gone"}) == {
        "ok": True
    }
    assert not (agents / "gone").exists()
    assert (agents / "kept" / "agent.yaml").exists()
    assert (tmp_path / ".fantastic" / "agent.yaml").exists()
    # the live tree is untouched — forget is disk only
    assert "gone" in root.ctx.agents


async def test_persist_record_merges_onto_the_live_agents_dir(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    root.create("file_bridge.tools", id="pr", allow=["."])
    f = tmp_path / ".fantastic" / "agents" / "pr" / "agent.yaml"
    f.parent.mkdir(parents=True)
    f.write_text("id: pr\nunknown: kept\n")
    out = await _root_call(
        root, {"type": "persist_record", "record": {"id": "pr", "x": 1}}
    )
    assert out == {"ok": True}
    assert yaml.safe_load(f.read_text()) == {"id": "pr", "unknown": "kept", "x": 1}
    assert (f.parent / "readme.md").exists() is False  # no handler_module named


async def test_blob_verbs_refuse_bad_arguments(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    base = {"agent_id": "kernel_state", "name": "s.yaml"}
    assert await _root_call(root, {"type": "persist_blob", "name": "s.yaml"}) == {
        "error": "persist_blob: agent_id (str) required"
    }
    assert await _root_call(root, {"type": "persist_blob", **base}) == {
        "error": "persist_blob: content (str) required"
    }
    assert await _root_call(root, {"type": "persist_blob", **base, "content": "x"}) == {
        "error": "persist_blob: no store wired — persistence not available"
    }
    assert await _root_call(root, {"type": "load_blob", "name": "s.yaml"}) == {
        "error": "load_blob: agent_id (str) required"
    }
    assert await _root_call(
        root, {"type": "load_blob", "agent_id": "kernel_state", "name": ""}
    ) == {"error": "load_blob: name (non-empty str) required"}
