"""The ONE persistence algorithm kernel_state implements TWICE — once straight
on `fs` (`write_record`, the cold bootstrap path) and once THROUGH a discovered
file_bridge store (`_persist_via_store`, the auto-flush) — pinned on both sinks.

Each sink writes the RECORD only — a bundle's readme and `docs/` are served from
the bundle, never copied — with the merge that keeps unknown
on-disk keys (a record larger than one 64 KiB read_stream chunk included) and the
commit that never leaves a torn record.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import pytest
import yaml

import file_bridge.tools as fbt
import kernel_state.tools as kst
from file_bridge import fs
from kernel_state.tools import on_shutdown, write_record

from _testkit import boot_root


def _tree(d: Path) -> list[str]:
    return sorted(str(p.relative_to(d)) for p in d.rglob("*") if p.is_file())


# ─── the direct sink (write_record) ──────────────────────────────────────────


def test_direct_sink_writes_the_record_only(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    rec = {"id": "rec", "handler_module": "kernel_state.tools"}
    write_record(fs.Root(tmp_path, Path("rec")), rec)
    assert _tree(tmp_path / "rec") == ["agent.yaml"]


def test_direct_sink_merge_keeps_unknown_keys_and_key_order(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    d = tmp_path / "m"
    d.mkdir()
    (d / "agent.yaml").write_text("id: m\nunknown: kept\nx: 1\n")
    write_record(fs.Root(tmp_path, Path("m")), {"id": "m", "x": 2, "y": 3})
    on_disk = yaml.safe_load((d / "agent.yaml").read_text(encoding="utf-8"))
    assert on_disk == {"id": "m", "unknown": "kept", "x": 2, "y": 3}
    assert list(on_disk) == ["id", "unknown", "x", "y"]


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


async def test_store_sink_writes_records_only(wired, tmp_path):
    root = wired
    root.ctx.update(root.id, note="touch")  # an `updated` event → persist via store
    root.create("kernel_state.tools", id="session", watch=False, root="sess")
    await _flush(root)
    d = tmp_path / ".fantastic"
    assert (
        yaml.safe_load((d / "agent.yaml").read_text(encoding="utf-8"))["note"]
        == "touch"
    )
    assert not (d / "readme.md").exists() and not (d / "docs").exists()
    assert _tree(d / "agents" / "session") == ["agent.yaml"]
    # a session loader keeps no copy of its record in its namespace
    assert not (tmp_path / "sess" / "agent.yaml").exists()


# twin: store_sink_merge_keeps_unknown_keys_past_one_chunk
async def test_persist_merge_preserves_extra_fields_on_disk(wired, tmp_path):
    root = wired
    root.create("file_bridge.tools", id="big", allow=["."])
    await _flush(root)
    f = tmp_path / ".fantastic" / "agents" / "big" / "agent.yaml"
    on_disk = yaml.safe_load(f.read_text(encoding="utf-8"))
    # an operator-added key big enough that the record spans two read_stream chunks,
    # and one more unknown key AFTER it (only a paged read reaches it)
    on_disk["blob"] = "x" * 70_000
    on_disk["tail_key"] = "kept"
    f.write_text(yaml.safe_dump(on_disk, sort_keys=False))
    assert f.stat().st_size > 65_536
    root.ctx.update("big", y=2)
    await _flush(root)
    after = yaml.safe_load(f.read_text(encoding="utf-8"))
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
    after = yaml.safe_load(f.read_text(encoding="utf-8"))
    assert after["y"] == 5 and after["id"] == "torn"


async def test_store_sink_a_refused_write_leaves_the_old_record(
    wired, tmp_path, capsys
):
    root = wired
    root.create("file_bridge.tools", id="keep", allow=["."], v=1)
    await _flush(root)
    f = tmp_path / ".fantastic" / "agents" / "keep" / "agent.yaml"
    before = f.read_text(encoding="utf-8")
    root.ctx.update("store", readonly=True)
    await _flush(root)  # the store's own update is refused too
    capsys.readouterr()
    root.ctx.update("keep", v=2)
    await _flush(root)
    assert f.read_text(encoding="utf-8") == before
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
    before = (d / "agent.yaml").read_text(encoding="utf-8")

    async def refuse(id, payload, kernel):
        return {"error": "rename: disk full", "reason": "bad_arg"}

    monkeypatch.setitem(fbt.VERBS, "rename", refuse)
    root.ctx.update("mv", v=2)
    await _flush(root)
    assert (d / "agent.yaml").read_text(encoding="utf-8") == before
    debris = [p.name for p in d.iterdir() if p.name.startswith("agent.yaml.tmp.")]
    assert len(debris) == 1, debris
    assert yaml.safe_load((d / debris[0]).read_text(encoding="utf-8"))["v"] == 2
    assert (
        "  [kernel_state] persist through 'store' failed: agents/mv/agent.yaml: "
        "rename: disk full"
    ) in capsys.readouterr().err


async def test_delete_removes_record_through_provider(wired, tmp_path):
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


async def test_an_agent_added_and_removed_between_drains_forgets_nothing(
    wired, tmp_path, capsys
):
    """An agent created and deleted before any drain took its write has no dir on
    disk: the drain neither writes it nor asks the store to delete it — no
    `persist through … does not exist` for a dir nobody wrote. Nested too."""
    root = wired
    await _flush(root)
    capsys.readouterr()
    root.create("file_bridge.tools", id="brief", allow=["."])
    root.ctx.agents["brief"].create("file_bridge.tools", id="inner", allow=["."])
    out = (
        await root.send(
            [{"target": "kernel", "payload": {"type": "delete_agent", "id": "brief"}}]
        )
    )[0]
    assert out["status"] == "completed", out
    await _flush(root)
    assert "persist through" not in capsys.readouterr().err
    assert not (tmp_path / ".fantastic" / "agents" / "brief").exists()
    assert root._persist_error is None


async def test_store_sink_a_removed_racing_its_first_write_still_forgets(
    wired, tmp_path, monkeypatch
):
    """A `removed` that lands while a drain is writing the agent's FIRST record
    still removes the dir that write leaves behind."""
    root = wired
    real = kst._persist_via_store
    took, release = asyncio.Event(), asyncio.Event()

    async def held(agent, store_id, abs_path, record):
        if record.get("id") == "racer":
            took.set()
            await release.wait()
        await real(agent, store_id, abs_path, record)

    monkeypatch.setattr(kst, "_persist_via_store", held)
    root.create("file_bridge.tools", id="racer", allow=["."])
    await asyncio.wait_for(took.wait(), 2)  # the debounced drain holds the write
    out = (
        await root.send(
            [{"target": "kernel", "payload": {"type": "delete_agent", "id": "racer"}}]
        )
    )[0]
    assert out["status"] == "completed", out
    release.set()
    await _flush(root)
    assert not (tmp_path / ".fantastic" / "agents" / "racer").exists()


async def test_a_flush_waits_for_the_drain_the_writer_already_took(
    wired, tmp_path, monkeypatch
):
    """`aflush` returns once everything dirty has LANDED — also what the debounced
    loop took first. Past the 0.15 s debounce the loop's own drain holds the probe's
    record; a flush that found the queue empty must not return with that write
    still in flight. The loop's write
    is held here, so the overlap is certain, not a timing window."""
    root = wired
    real = kst._persist_via_store
    took, release = asyncio.Event(), asyncio.Event()

    async def held(agent, store_id, abs_path, record):
        if record.get("id") == "probe":
            took.set()
            await release.wait()
        await real(agent, store_id, abs_path, record)

    monkeypatch.setattr(kst, "_persist_via_store", held)
    root.create("kernel_state.tools", id="probe", watch=False, root="sess")
    await asyncio.wait_for(took.wait(), 2)  # the debounced loop's drain has it
    flush = asyncio.create_task(root._fs_flush_loop.aflush())
    await asyncio.sleep(0.05)
    assert not flush.done(), "aflush returned with the loop's write in flight"
    release.set()
    await asyncio.wait_for(flush, 2)
    d = tmp_path / ".fantastic" / "agents" / "probe"
    assert _tree(d) == ["agent.yaml"]


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
    assert yaml.safe_load(f.read_text(encoding="utf-8")) == {
        "id": "pr",
        "unknown": "kept",
        "x": 1,
    }
    assert (f.parent / "readme.md").exists() is False  # no handler_module named


async def test_blob_verbs_are_the_loaders_and_refuse_like_python(tmp_path, monkeypatch):
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


# ─── the dirty set and the shape of the store traffic ────────────────────────


async def test_records_created_before_the_store_persist_once_it_is_wired(
    tmp_path, monkeypatch
):
    """The record writer's DIRTY SET SURVIVES a no-store flush: it returns BEFORE
    clearing the queues when nothing is wired, so everything created before a store
    exists lands the moment one does."""
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    (await root.send([{"target": root.id, "payload": {"type": "boot"}}]))[0]
    try:
        # NO store yet: the record is queued, not written, and NOT dropped.
        root.create("file_bridge.tools", id="early", mode="mem")
        await _flush(root)
        assert not (tmp_path / ".fantastic/agents/early/agent.yaml").exists()

        # Wire one: the queued record drains on the next flush.
        root.create(
            "file_bridge.tools",
            allow=["."],
            id="store",
            root=".fantastic",
            ingress_rule="allow_all_ingress",
        )
        await _flush(root)
        assert (tmp_path / ".fantastic/agents/early/agent.yaml").exists(), (
            "the dirty set must survive a no-store flush"
        )
    finally:
        await on_shutdown(root)


async def test_store_calls_have_python_shape(wired):
    """The store calls are TEXT-channel dicts: a read is `{type: read_stream, path,
    offset}` with no `bytes` key, a write `{type: write_stream, path, bytes,
    truncate}` with the chunk at `bytes` in place."""
    root = wired
    seen: list[dict] = []
    root.ctx.add_state_subscriber(
        lambda e: (
            seen.append(e["payload"])
            if e.get("agent_id") == "store" and e.get("kind") == "send"
            else None
        )
    )
    root.create("file_bridge.tools", id="shaped", mode="mem")
    await _flush(root)  # a barrier: every dirty write has landed

    read = next(
        p
        for p in seen
        if p.get("type") == "read_stream"
        and p.get("path") == "agents/shaped/agent.yaml"
    )
    assert list(read) == ["type", "path", "offset"], read
    write = next(
        p
        for p in seen
        if p.get("type") == "write_stream"
        and str(p.get("path")).startswith("agents/shaped/agent.yaml.tmp.")
    )
    assert list(write) == ["type", "path", "bytes", "truncate"], write
