"""sqlite_store: what only a durable store has — the file, its lifecycle, and a
kernel restart that reads every table back.

The verbs themselves are ram_store's suite (`ram_store/tests/test_ram_store.py`
runs every test against both stores) and the shared memory-store cases
(`test_memory_store_cases.py`)."""

from __future__ import annotations

import sqlite3
import types
from pathlib import Path

import pytest

from _testkit import as_agent, boot_root, persist, posix_only, wire
from kernel import register_unlisted
from kernel._kernel import shutdown_context

DB = "stores/mem.db"


async def _call(root, target, payload, *, as_=None, **item):
    batch = [{"target": target, "payload": payload, **item}]
    if as_ is None:
        return (await root.send(batch))[0]
    with as_agent(root, as_):
        return (await root.send(batch))[0]


async def _create(root, **meta):
    payload = {"type": "create_agent", "parent_id": root.id, **meta}
    return (await _call(root, root.id, payload))["result"]


@pytest.fixture
async def mem(kernel):
    kernel.ctx.update(kernel.id, guid="g1")
    rec = kernel.create(None, id="alice", can_call={"mem": "*"})
    assert rec.get("id") == "alice", rec
    rec = kernel.create(
        "sqlite_store.tools", id="mem", path=DB, callable_by={"alice": "*"}
    )
    assert rec.get("id") == "mem", rec
    return "mem"


# ── the record's `path` ─────────────────────────────────────────────────


@pytest.mark.parametrize(
    "path, says",
    [
        (None, "needs `path`"),
        ("", "needs `path`"),
        ("/tmp/x.db", "must stay inside the project dir"),
        ("../x.db", "must stay inside the project dir"),
        ("a/../../x.db", "must stay inside the project dir"),
    ],
)
async def test_path_is_required_relative_and_inside(kernel, path, says):
    meta = {"handler_module": "sqlite_store.tools"}
    if path is not None:
        meta["path"] = path
    out = await _create(kernel, **meta)
    assert out["reason"] == "invalid_meta" and says in out["error"], out


async def test_path_cannot_be_dropped_by_update(kernel, mem):
    out = (
        await _call(
            kernel, kernel.id, {"type": "update_agent", "id": "mem", "path": ""}
        )
    )["result"]
    assert out["reason"] == "invalid_meta", out


@posix_only("creating a directory symlink needs a privilege on Windows")
async def test_a_symlink_out_of_the_dir_is_refused_at_boot(kernel, tmp_path):
    outside = tmp_path.parent / f"{tmp_path.name}-outside"
    outside.mkdir()
    Path("stores").symlink_to(outside, target_is_directory=True)
    kernel.ctx.update(kernel.id, guid="g1")
    kernel.create("sqlite_store.tools", id="mem", path=DB)
    out = (await _call(kernel, "mem", {"type": "boot"}))["result"]
    assert out["reason"] == "bad_arg" and out["arg"] == "path", out
    assert not (outside / "mem.db").exists()


# ── the file ────────────────────────────────────────────────────────────


async def test_boot_opens_the_file_with_the_rust_stores_pragmas(kernel, mem):
    assert (await _call(kernel, "mem", {"type": "boot"}))["result"] is None
    assert Path(DB).is_file()
    c = kernel.ctx.agents["mem"]._sqlite_conn
    assert c.execute("PRAGMA journal_mode").fetchone()[0] == "wal"
    assert c.execute("PRAGMA synchronous").fetchone()[0] == 1  # NORMAL
    assert c.execute("PRAGMA busy_timeout").fetchone()[0] == 5000
    assert c.execute("PRAGMA foreign_keys").fetchone()[0] == 1
    tables = {
        r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE type='table'")
    }
    assert {"rows", "ref_lists", "fold_refs", "text_idx"} <= tables


async def test_reflect_is_durable_and_names_its_file(kernel, mem):
    await _call(kernel, "mem", {"type": "append", "rows": [{"id": 1}]}, as_="alice")
    out = (await _call(kernel, "mem", {"type": "reflect"}))["result"]
    own = {k: out[k] for k in list(out)[:7]}
    assert own == {
        "id": "mem",
        "sentence": "Memory store in SQLite — rows per owner (the caller), folds, "
        "search; durable: one database file, the tables survive the process.",
        "durable": True,
        "path": DB,
        "owners": 1,
        "rows": 1,
        "optional_verbs": ["search"],
    }


async def test_a_row_that_is_not_json_is_refused_whole(kernel, mem):
    out = (
        await _call(
            kernel,
            "mem",
            {"type": "append", "rows": [{"id": 1}, {"id": 2, "x": float("nan")}]},
            as_="alice",
        )
    )["result"]
    assert out["reason"] == "bad_arg" and "row 2 is not JSON" in out["error"], out
    rows = (await _call(kernel, "mem", {"type": "read"}, as_="alice"))["result"]
    assert rows["rows"] == [], "all or none"


async def test_a_ram_store_dump_injects_here_and_back(kernel, mem):
    kernel.create("ram_store.tools", id="ram", callable_by={"alice": "*"})
    wire(kernel, "alice", "ram")
    rows = [
        {"id": 1, "role": "user", "content": "hi", "name": "job"},
        {"id": 2, "content": {"nested": [1, 2]}, "title": "t"},
        {"id": 3, "kind": "fold", "refs": [1, 2], "content": "[fold]"},
        {"id": 4, "kind": "fold", "refs": [], "content": "empty"},
    ]
    await _call(kernel, "ram", {"type": "append", "rows": rows}, as_="alice")
    tables = (await _call(kernel, "ram", {"type": "dump"}))["result"]["tables"]
    out = (await _call(kernel, "mem", {"type": "inject", "tables": tables}))["result"]
    assert out == {"rows": 4}
    back = (await _call(kernel, "mem", {"type": "dump"}))["result"]["tables"]
    assert back == tables
    await _call(kernel, "ram", {"type": "inject", "tables": back})
    assert (await _call(kernel, "ram", {"type": "dump"}))["result"]["tables"] == tables


async def test_delete_agent_deletes_the_file_and_its_wal(kernel, mem):
    await _call(kernel, "mem", {"type": "append", "rows": [{"id": 1}]}, as_="alice")
    assert Path(DB).is_file() and Path(DB + "-wal").exists()
    out = (await _call(kernel, kernel.id, {"type": "delete_agent", "id": "mem"}))[
        "result"
    ]
    assert out.get("deleted") is True or "error" not in out, out
    for suffix in ("", "-wal", "-shm"):
        assert not Path(DB + suffix).exists(), suffix


async def test_a_stop_checkpoints_and_keeps_the_file(kernel, mem):
    await _call(kernel, "mem", {"type": "append", "rows": [{"id": 1}]}, as_="alice")
    with shutdown_context():
        await kernel.ctx.shutdown()
    assert Path(DB).is_file()
    assert not Path(DB + "-wal").exists(), "the WAL was checkpointed into the file"
    c = sqlite3.connect(DB)
    assert c.execute("SELECT COUNT(*) FROM rows").fetchone()[0] == 1
    c.close()


# ── a kernel restart ────────────────────────────────────────────────────

_counts: dict[str, int] = {}


async def _probe(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "reflect"):
        return None if verb == "boot" else {"id": id, "sentence": "probe"}
    if verb == "count":
        _counts[id] = _counts.get(id, 0) + 1
        return {"n": _counts[id]}
    if verb == "fail":
        raise RuntimeError("probe failed on purpose")
    return {"error": f"unknown {verb!r}", "reason": "unhandled"}


_mod = types.ModuleType("sqlite_probe.tools")
_mod.handler = _probe
_mod.VERBS = {"reflect": None, "count": None, "fail": None}
register_unlisted("sqlite_probe.tools", _mod)


async def test_every_table_survives_a_kernel_restart(tmp_path, monkeypatch):
    """Rows, folds, the ledger (a dead letter) and named-call dedup are read back
    by a NEW kernel on the same dir and guid — no dump, no inject."""
    monkeypatch.chdir(tmp_path)
    root = boot_root()
    await _call(root, root.id, {"type": "boot"})  # mints the guid
    for meta in (
        {"id": "memory", "handler_module": "sqlite_store.tools", "path": DB},
        {"id": "probe", "handler_module": "sqlite_probe.tools"},
        {"id": "ai", "handler_module": "sqlite_probe.tools"},
    ):
        out = await _create(root, **meta)
        assert out.get("id") == meta["id"], out
    wire(root, "ai", "memory")
    root.ctx.update(root.id, memory_store_id="memory")
    assert (await root.ctx.start(boot_all=False)).get("started")

    # an AI agent's conversation and a fold over it
    rows = [
        {"id": 900001, "role": "user", "content": "the audit marker is 7"},
        {"id": 900002, "role": "assistant", "content": "noted"},
        {"id": 900003, "kind": "fold", "refs": [900001, 900002], "content": "[fold]"},
    ]
    out = await _call(root, "memory", {"type": "append", "rows": rows}, as_="ai")
    assert out["result"]["appended"] == 3, out
    # a named call (dedup) and a dead letter (the ledger)
    _counts.clear()
    first = await _call(root, "probe", {"type": "count"}, id="nightly")
    dead = await _call(root, "probe", {"type": "fail"})
    assert dead["status"] == "failed"
    guid = root.record["guid"]
    persist(root)
    with shutdown_context():
        await root.ctx.shutdown()
    assert not Path(DB + "-wal").exists(), "the stop checkpointed the WAL"

    # A NEW kernel on the same dir: same guid, the same file — nothing injected.
    root2 = boot_root()
    assert root2.record["guid"] == guid
    assert (await root2.ctx.start(boot_all=False)).get("started")
    got = (await _call(root2, "memory", {"type": "read"}, as_="ai"))["result"]
    assert got["rows"] == rows
    found = (
        await _call(
            root2,
            "memory",
            {"type": "search", "query": "AUDIT", "ids": [900003]},
            as_="ai",
        )
    )["result"]
    assert found["matches"] == [{"id": 900001, "line": 1, "text": rows[0]["content"]}]
    led = (await _call(root2, "kernel", {"type": "ledger"}))["result"]
    entry = next(e for e in led["executions"] if e["id"] == dead["pid"])
    assert entry["status"] == "failed" and not entry.get("acked"), entry
    again = await _call(root2, "probe", {"type": "count"}, id="nightly")
    assert again["result"] == first["result"] == {"n": 1}
    assert _counts["probe"] == 1, "the named step never ran twice"
    with shutdown_context():
        await root2.ctx.shutdown()


async def test_verb_docs_are_ram_stores_but_boot(kernel, mem):
    """HOW TO CALL it is ram_store's, word for word; only `boot` names the file."""
    kernel.create("ram_store.tools", id="ram")

    async def docs(of):
        payload = {"type": "docs", "of": of, "tier": "detailed"}
        return (await _call(kernel, "kernel", payload))["result"]["verbs"]

    ram, sql = await docs("ram"), await docs("mem")
    assert list(sql) == list(ram)
    assert {k: v for k, v in sql.items() if k != "boot"} == {
        k: v for k, v in ram.items() if k != "boot"
    }
    assert "SQLite file" in sql["boot"]
