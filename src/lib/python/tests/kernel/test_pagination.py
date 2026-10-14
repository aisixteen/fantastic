"""Every unbounded list reply can be asked for less.

WHY THIS EXISTS. Unbounded replies are a real cost, not a conformance box. Reflect
bulk can be a large share of an AI agent's context; `dedupe_stale_reads`, the
reflect stub and the point-of-use distillation nudge in `ai_core` shrink what is
RETAINED, and paging shrinks what is SENT. Paging attacks the cause.

The canonical shape is `checkpoint-conformance/spec/test_list.py` (limit
respected, opaque cursor, filters compose), sharpened by a2a-tck's `historyLength`
MUST: a server MUST NOT exceed what the caller asked for.
"""

from __future__ import annotations

import pytest

from kernel import page


def _rows(n: int) -> list[dict]:
    return [{"id": f"a{i:02d}"} for i in range(n)]


_BY_ID = {"key": lambda r: r["id"]}


# ─── the primitive ──────────────────────────────────────────────


def test_absent_limit_returns_everything_so_paging_is_inert_until_asked_for():
    """The default MUST be unchanged. A caller that never learned about paging
    gets exactly what it always got — otherwise adding the field silently
    truncates every existing client."""
    rows = _rows(50)
    got, nxt = page(rows, {}, **_BY_ID)
    assert got == rows
    assert nxt is None


def test_a_limit_is_never_exceeded():
    got, nxt = page(_rows(50), {"limit": 7}, **_BY_ID)
    assert len(got) == 7
    assert nxt is not None


def test_the_cursor_walks_the_whole_list_exactly_once_and_then_stops():
    """Termination is the half that gets forgotten: a cursor that never returns
    None is an infinite loop in the caller."""
    rows, seen, cursor, hops = _rows(23), [], None, 0
    while True:
        payload = {"limit": 5} | ({"after": cursor} if cursor else {})
        got, cursor = page(rows, payload, **_BY_ID)
        seen.extend(got)
        hops += 1
        if cursor is None:
            break
        assert hops < 20, "cursor never terminated"
    assert seen == rows, "the walk must see every row, in order, exactly once"
    assert hops == 5  # 23 rows / 5


def test_the_cursor_is_opaque_and_not_an_offset():
    """An offset is arithmetic a caller can do itself — and then it is a
    contract you cannot change. The cursor names the last item it handed over,
    so the only way to get page N+1 is to have been given page N."""
    _, cursor = page(_rows(10), {"limit": 3}, **_BY_ID)
    assert cursor == "a02"
    with pytest.raises((ValueError, TypeError)):
        int(cursor)


def test_a_cursor_naming_something_gone_yields_nothing_rather_than_restarting():
    """The dangerous failure is not an error, it is a SILENT RESTART: a caller
    resuming with a stale cursor would re-walk the whole list from the top and
    read it as new work. Over a live tree a cursor may miss an item deleted
    mid-walk — that is stated, and is why this kernel does not pretend to hand
    out snapshots."""
    got, nxt = page(_rows(10), {"limit": 3, "after": "deleted-since"}, **_BY_ID)
    assert got == []
    assert nxt is None


@pytest.mark.parametrize("bad", [0, -1, "3", 2.5, True, None])
def test_a_limit_that_is_not_a_positive_int_pages_nothing(bad):
    """Including `True`, which `isinstance(x, int)` accepts and which would
    otherwise silently mean "one row"."""
    rows = _rows(10)
    got, nxt = page(rows, {"limit": bad}, **_BY_ID)
    assert got == rows and nxt is None


# ─── the verbs that use it ──────────────────────────────────────


async def test_list_agents_pages_and_the_walk_reassembles_the_whole_registry(kernel):
    for i in range(6):
        (
            await kernel.send(
                [
                    {
                        "target": kernel.id,
                        "payload": {
                            "type": "create_agent",
                            "parent_id": kernel.id,
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                            "id": f"fb{i}",
                        },
                    }
                ]
            )
        )[0]["result"]
    whole = (
        await kernel.send([{"target": kernel.id, "payload": {"type": "list_agents"}}])
    )[0]["result"]
    assert whole["next"] is None, "an unpaged call still returns everything"

    seen, cursor = [], None
    while True:
        payload = {"type": "list_agents", "limit": 2} | (
            {"after": cursor} if cursor else {}
        )
        r = (await kernel.send([{"target": kernel.id, "payload": payload}]))[0][
            "result"
        ]
        assert len(r["agents"]) <= 2
        seen.extend(a["id"] for a in r["agents"])
        cursor = r["next"]
        if cursor is None:
            break
    assert seen == [a["id"] for a in whole["agents"]]


async def test_ps_pages_the_task_table(kernel):
    """`ps` is the other kernel-wide list, and the one that grows with LOAD
    rather than with composition — the case where a cap matters most."""
    r = (
        await kernel.send(
            [{"target": kernel.id, "payload": {"type": "ps", "limit": 1}}]
        )
    )[0]["result"]
    assert len(r["tasks"]) <= 1
    assert "next" in r


async def test_reflect_pages_the_flat_id_index_but_never_the_nested_tree(kernel):
    """`tree:"ids"` is a list and pages. `tree:"all"` is a nested subtree whose
    VALUE is its structure — slicing it would hand back a forest with no root and
    call it a page, so it is left whole. Reflect must also stay useful in ONE
    round-trip, so the addressed agent's own identity and verbs never page."""
    for i in range(5):
        (
            await kernel.send(
                [
                    {
                        "target": kernel.id,
                        "payload": {
                            "type": "create_agent",
                            "parent_id": kernel.id,
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                            "id": f"n{i}",
                        },
                    }
                ]
            )
        )[0]["result"]
    r = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "reflect", "tree": "ids", "limit": 3},
                }
            ]
        )
    )[0]["result"]
    assert len(r["tree"]) == 3
    assert r["next"]
    assert r["id"] and r["sentence"], "identity is never paged away"

    whole = (
        await kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "tree": "all"}}]
        )
    )[0]["result"]
    assert isinstance(whole["tree"], dict), "the nested tree is not a list"
    assert "next" not in whole
