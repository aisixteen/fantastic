"""The kernel row store: rows, bodies, views — and the `messages` verb's in-handler gate."""

from __future__ import annotations


import pytest

from _testkit import boot_root
from kernel import sender_context


async def _send(root, target, payload):
    return (await root.send([{"target": target, "payload": payload}]))[0]["result"]


async def _mk(r0, aid, **meta):
    r = await _send(
        r0,
        r0.id,
        {
            "type": "create_agent",
            "parent_id": r0.id,
            "id": aid,
            "handler_module": "text_file.tools",
            "target_id": "fb",
            **meta,
        },
    )
    assert not (isinstance(r, dict) and r.get("error")), r


@pytest.fixture
def root(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    return boot_root()


def test_rows_put_mints_in_the_execution_id_space_and_keeps_order(root):
    k = root.ctx
    a = k.rows_put("ai", {"role": "user", "content": "hello"})
    b = k.rows_put("ai", {"role": "assistant", "content": "hi", "parent": a})
    assert isinstance(a, int) and b == a + 1, "the one counter, in order"
    assert [r["id"] for r in k.rows_for("ai")] == [a, b]
    assert k.rows_for("nobody") == []
    assert k.rows_get([b, 999999, a]) == [k.rows[b], k.rows[a]]
    assert k.rows[a]["agent"] == "ai" and "ts" in k.rows[a]


def test_a_row_id_never_collides_with_a_live_task(root):
    k = root.ctx
    ids = {k.rows_put("ai", {"role": "user", "content": str(i)}) for i in range(50)}
    assert len(ids) == 50 and not (ids & set(k.tasks))


def test_bodies_page_and_views_round_trip(root):
    k = root.ctx
    k.body_put(1, b"0123456789")
    assert k.body_get(1, 0, 4) == (b"0123", 4, False, 10)
    assert k.body_get(1, 8, 4) == (b"89", 10, True, 10)
    assert k.body_get(999, 0, 4) is None
    k.view_put("ai", {"v": 1, "ids": [{"id": "x", "res": "full"}]})
    assert k.view_get("ai")["ids"][0]["id"] == "x" and k.view_get("zz") is None


async def test_rows_outlive_delete_agent(root):
    await _mk(root, "fb", handler_module="file_bridge.tools", root=".")
    k = root.ctx
    rid = k.rows_put("ai", {"role": "user", "content": "kept"})
    await _mk(root, "ai")
    await _send(root, root.id, {"type": "delete_agent", "id": "ai"})
    assert k.rows_get([rid]) and k.rows_for("ai")


async def test_messages_operator_sees_all_and_answers_digests_not_bodies(root):
    k = root.ctx
    a = k.rows_put("ai", {"role": "user", "content": "x" * 5000, "kind": "reply"})
    f = k.rows_put(
        "ai",
        {
            "role": "user",
            "kind": "fold",
            "title": "done",
            "refs": [a],
            "level": 1,
            "content": "[fold]",
        },
    )
    r = await _send(root, "kernel", {"type": "messages", "ids": [a, f, 999999]})
    assert r["refused"] == [999999]
    assert [d["id"] for d in r["rows"]] == [a, f]
    assert r["rows"][0]["bytes"] == 5000 and len(r["rows"][0]["head"]) <= 120
    assert (
        r["rows"][1]["title"] == "done"
        and r["rows"][1]["refs"] == 1
        and r["rows"][1]["level"] == 1
    )


async def test_messages_own_rows_always_a_peers_only_by_grant(root):
    await _mk(root, "fb", handler_module="file_bridge.tools", root=".")
    await _mk(root, "ai", can_call={"kernel_state": ["messages"]})
    await _mk(root, "peer", can_call={"kernel_state": ["messages"]})
    k = root.ctx
    mine = k.rows_put("ai", {"role": "user", "content": "private"})
    theirs = k.rows_put("peer", {"role": "user", "content": "peer's"})
    # the AI reads its own, is refused the peer's
    with sender_context("ai"):
        r = await _send(root, "kernel", {"type": "messages", "ids": [mine, theirs]})
    assert [d["id"] for d in r["rows"]] == [mine] and r["refused"] == [theirs]
    # the peer is refused the AI's — until the AI's callable_by names it for `messages`
    with sender_context("peer"):
        r = await _send(root, "kernel", {"type": "messages", "ids": [mine]})
    assert r["rows"] == [] and r["refused"] == [mine]
    await _send(
        root,
        root.id,
        {"type": "update_agent", "id": "ai", "callable_by": {"peer": ["messages"]}},
    )
    with sender_context("peer"):
        r = await _send(root, "kernel", {"type": "messages", "ids": [mine]})
    assert [d["id"] for d in r["rows"]] == [mine] and r["refused"] == []


async def test_messages_pages_and_caps(root):
    k = root.ctx
    ids = [k.rows_put("ai", {"role": "user", "content": "y" * 200}) for _ in range(400)]
    r = await _send(root, "kernel", {"type": "messages", "ids": ids, "limit": 5})
    assert len(r["rows"]) == 5 and r["next"] == ids[4]
    r2 = await _send(
        root, "kernel", {"type": "messages", "ids": ids, "after": r["next"], "limit": 5}
    )
    assert r2["rows"][0]["id"] == ids[5]
    big = await _send(root, "kernel", {"type": "messages", "ids": ids})
    assert len(big["rows"]) < 400 and big["next"], "the 32KB cap paged it"


async def test_messages_refuses_a_bad_ids_arg(root):
    r = await _send(root, "kernel", {"type": "messages"})
    assert r["error"] and r["reason"] == "bad_arg"
