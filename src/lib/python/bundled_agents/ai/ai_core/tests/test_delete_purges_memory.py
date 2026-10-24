"""delete_agent removes the deleted AI agent's rows from its memory store — its
conversation, its folds (and their refs) and the search index — unless the delete
asks `keep_memory:true`. The store is a durable sqlite_store, so what the test
reads back is what a restart would find."""

from __future__ import annotations

from _testkit import as_agent, memory_meta

TOKEN = "zq9deletedTOKEN"


def _sqlite_store(root, store: str = "mem") -> str:
    if not root.record.get("guid"):
        root.ctx.update(root.id, guid="test-guid")
    root.create("sqlite_store.tools", id=store, path=f"{store}.db")
    return store


def _ai(root, aid: str, store: str = "mem") -> None:
    meta = memory_meta(root, aid, {"cli": "*", "kernel": "*"}, store=store)
    root.create("ollama_backend.tools", id=aid, context_window=32768, **meta)


async def _plant(root, aid: str, store: str = "mem") -> None:
    """Three rows (one carrying the token), then a fold over the first two."""
    rows = [
        {"id": 1, "role": "user", "content": f"hello {TOKEN}"},
        {"id": 2, "role": "assistant", "content": "a reply"},
        {"id": 3, "kind": "fold", "role": "user", "refs": [1, 2], "content": "[fold]"},
    ]
    with as_agent(root, aid):
        out = (
            await root.send(
                [{"target": store, "payload": {"type": "append", "rows": rows}}]
            )
        )[0]
    assert out["status"] == "completed", out


async def _tables(root, store: str = "mem") -> dict:
    out = (await root.send([{"target": store, "payload": {"type": "dump"}}]))[0][
        "result"
    ]
    return out["tables"]


def _owned(tables: dict, aid: str) -> tuple[list, list]:
    """The rows and fold refs the store keys under agent `aid` (`<guid>|<aid>`)."""
    rows = [r for r in tables["rows"] if r["owner"].endswith(f"|{aid}")]
    folds = [f for f in tables["fold_refs"] if f["owner"].endswith(f"|{aid}")]
    return rows, folds


async def _search_total(root, aid: str, store: str = "mem") -> int:
    with as_agent(root, aid):
        out = (
            await root.send(
                [{"target": store, "payload": {"type": "search", "query": TOKEN}}]
            )
        )[0]
    assert out["status"] == "completed", out
    return out["result"]["total"]


async def _delete(root, payload: dict) -> dict:
    out = await root.send(
        [{"target": root.id, "payload": {"type": "delete_agent", **payload}}]
    )
    return out[0]["result"]


async def test_delete_agent_removes_its_rows_and_index(seeded_kernel):
    _sqlite_store(seeded_kernel)
    _ai(seeded_kernel, "ai")
    await _plant(seeded_kernel, "ai")
    assert await _search_total(seeded_kernel, "ai") > 0
    rows, folds = _owned(await _tables(seeded_kernel), "ai")
    assert len(rows) == 3 and [(f["fold"], f["members"]) for f in folds] == [
        (3, [1, 2])
    ]

    out = await _delete(seeded_kernel, {"id": "ai"})
    assert out == {"deleted": True, "id": "ai"}
    rows, folds = _owned(await _tables(seeded_kernel), "ai")
    assert rows == [], "the conversation left the store"
    assert folds == [], "the fold refs left the store"
    _ai(seeded_kernel, "ai")  # a new agent under the same id starts clean
    assert await _search_total(seeded_kernel, "ai") == 0, "the index forgot it"


async def test_delete_agent_keep_memory_keeps_them(seeded_kernel):
    _sqlite_store(seeded_kernel)
    _ai(seeded_kernel, "ai")
    await _plant(seeded_kernel, "ai")

    out = await _delete(seeded_kernel, {"id": "ai", "keep_memory": True})
    assert out == {"deleted": True, "id": "ai"}
    rows, folds = _owned(await _tables(seeded_kernel), "ai")
    assert len(rows) == 3, "keep_memory keeps the rows"
    assert [(f["fold"], f["members"]) for f in folds] == [(3, [1, 2])]

    # Re-created under the same id, the kept rows are its memory; a plain delete
    # then purges them.
    _ai(seeded_kernel, "ai")
    assert await _search_total(seeded_kernel, "ai") > 0, "search still hits"
    await _delete(seeded_kernel, {"id": "ai"})
    rows, folds = _owned(await _tables(seeded_kernel), "ai")
    assert rows == [] and folds == []


async def test_rows_of_other_agents_are_untouched(seeded_kernel):
    _sqlite_store(seeded_kernel)
    _ai(seeded_kernel, "ai")
    _ai(seeded_kernel, "other")
    await _plant(seeded_kernel, "ai")
    await _plant(seeded_kernel, "other")

    await _delete(seeded_kernel, {"id": "ai"})
    rows, folds = _owned(await _tables(seeded_kernel), "other")
    assert len(rows) == 3 and len(folds) == 1, "the other agent keeps everything"
    assert await _search_total(seeded_kernel, "other") > 0


async def test_deleting_a_parent_purges_its_ai_child(seeded_kernel):
    _sqlite_store(seeded_kernel)
    seeded_kernel.create("file_bridge.tools", allow=["."], id="P")
    p = seeded_kernel.ctx.agents["P"]
    _ai(p, "child")
    await _plant(seeded_kernel, "child")
    assert len(_owned(await _tables(seeded_kernel), "child")[0]) == 3

    out = await _delete(seeded_kernel, {"id": "P"})
    assert out == {"deleted": True, "id": "P"}
    assert _owned(await _tables(seeded_kernel), "child") == ([], [])
