"""ram_store: tenancy by caller, fold refs + FK cascade, search, inject/dump.

Callers act through `as_agent` (the test standing in for the agent's handler — a
real agent send, gated both ways); the operator is a bare `kernel.send`. The root's guid is set by hand — the root is not
booted in these tests, and the store keys every row by it."""

from __future__ import annotations

import pytest

from _testkit import as_agent


async def _call(kernel, target, payload, *, as_=None):
    if as_ is None:
        return (await kernel.send([{"target": target, "payload": payload}]))[0][
            "result"
        ]
    with as_agent(kernel, as_):
        return (await kernel.send([{"target": target, "payload": payload}]))[0][
            "result"
        ]


@pytest.fixture
async def store(kernel):
    kernel.ctx.update(kernel.id, guid="g1")
    for aid in ("alice", "bob", "carol", "leg"):
        rec = kernel.create(None, id=aid, can_call={"mem": "*"})
        assert rec.get("id") == aid, rec
    rec = kernel.create(
        "ram_store.tools",
        id="mem",
        callable_by={"alice": "*", "bob": "*", "carol": "*", "leg": "*"},
    )
    assert rec.get("id") == "mem", rec
    return "mem"


async def _append(kernel, who, rows):
    return await _call(kernel, "mem", {"type": "append", "rows": rows}, as_=who)


async def test_rows_are_keyed_by_caller(kernel, store):
    out = await _append(kernel, "alice", [{"id": 101, "role": "user", "content": "hi"}])
    assert out == {"ids": [101], "appended": 1}
    # the same id is free under another owner — keys are (owner, id)
    assert (await _append(kernel, "bob", [{"id": 101, "content": "bob"}]))["ids"] == [
        101
    ]
    a = await _call(kernel, "mem", {"type": "read"}, as_="alice")
    b = await _call(kernel, "mem", {"type": "read"}, as_="bob")
    assert [r["content"] for r in a["rows"]] == ["hi"]
    assert [r["content"] for r in b["rows"]] == ["bob"]
    # a duplicate id under one owner is refused, all or none
    dup = await _append(kernel, "alice", [{"id": 102}, {"id": 101}])
    assert dup["reason"] == "exists"
    assert len((await _call(kernel, "mem", {"type": "read"}, as_="alice"))["rows"]) == 1


async def test_owner_is_never_a_writable_field(kernel, store):
    """There is no `owner` argument: a row lands under its caller's key whatever
    the payload names, and a read answers the caller's own rows."""
    await _append(kernel, "bob", [{"id": 9, "content": "bob's"}])
    out = await _call(
        kernel,
        "mem",
        {"type": "append", "owner": "bob", "rows": [{"id": 5}]},
        as_="alice",
    )
    assert out["ids"] == [5]
    bob = await _call(kernel, "mem", {"type": "read"}, as_="bob")
    assert [r["id"] for r in bob["rows"]] == [9]
    out = await _call(kernel, "mem", {"type": "read", "owner": "bob"}, as_="alice")
    assert [r["id"] for r in out["rows"]] == [5]


async def test_missing_id_takes_the_kernels_next(kernel, store):
    before = kernel.ctx.next_id
    out = await _append(kernel, "alice", [{"content": "x"}])
    assert out["ids"][0] >= before


async def test_the_operator_is_served_under_its_own_key(kernel, store):
    await _call(kernel, "mem", {"type": "append", "rows": [{"id": 7, "kind": "call"}]})
    got = await _call(kernel, "mem", {"type": "read", "kinds": ["call"]})
    assert [r["id"] for r in got["rows"]] == [7]
    # the operator's rows are its own key — alice sees none of them
    assert (await _call(kernel, "mem", {"type": "read"}, as_="alice"))["rows"] == []
    tables = (await _call(kernel, "mem", {"type": "dump"}))["tables"]
    assert [r["owner"] for r in tables["rows"]] == ["g1|kernel"]


async def test_refs_must_name_own_rows(kernel, store):
    await _append(kernel, "bob", [{"id": 10}])
    out = await _append(kernel, "alice", [{"id": 20, "kind": "fold", "refs": [10]}])
    assert out["reason"] == "not_found"
    # members in the same batch are fine
    out = await _append(
        kernel,
        "alice",
        [{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}],
    )
    assert out["ids"] == [1, 2, 3]


async def test_prune_cascades_to_fold_refs(kernel, store):
    await _append(
        kernel,
        "alice",
        [{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}],
    )
    out = await _call(kernel, "mem", {"type": "prune", "before_id": 2}, as_="alice")
    assert out == {"pruned": 1}
    fold = await _call(kernel, "mem", {"type": "read", "ids": [3]}, as_="alice")
    assert fold["rows"][0]["refs"] == [2]


async def test_search_lines_and_fold_descent(kernel, store):
    await _append(
        kernel,
        "alice",
        [
            {"id": 1, "content": "alpha\nthe audit marker is 7\nomega"},
            {"id": 2, "content": "nothing"},
            {"id": 3, "kind": "fold", "refs": [1, 2], "content": "[fold] survey"},
        ],
    )
    got = await _call(kernel, "mem", {"type": "search", "query": "AUDIT"}, as_="alice")
    assert got["matches"] == [{"id": 1, "line": 2, "text": "the audit marker is 7"}]
    scoped = await _call(
        kernel, "mem", {"type": "search", "query": "marker", "ids": [3]}, as_="alice"
    )
    assert scoped["total"] == 1


async def test_read_paging_and_selectors(kernel, store):
    await _append(
        kernel,
        "alice",
        [
            {"id": 1, "kind": "call", "name": "job"},
            {"id": 2, "kind": "clear"},
            {"id": 3},
            {"id": 4, "kind": "view"},
        ],
    )
    head = await _call(
        kernel, "mem", {"type": "read", "from_last_kind": "clear"}, as_="alice"
    )
    assert [r["id"] for r in head["rows"]] == [2, 3, 4]
    named = await _call(
        kernel, "mem", {"type": "read", "match": {"name": "job"}}, as_="alice"
    )
    assert [r["id"] for r in named["rows"]] == [1]
    last_view = await _call(
        kernel,
        "mem",
        {"type": "read", "kinds": ["view"], "order": "desc", "limit": 1},
        as_="alice",
    )
    assert [r["id"] for r in last_view["rows"]] == [4]
    page = await _call(kernel, "mem", {"type": "read", "limit": 2}, as_="alice")
    assert [r["id"] for r in page["rows"]] == [1, 2] and page["next"] == 2
    rest = await _call(kernel, "mem", {"type": "read", "after": 2}, as_="alice")
    assert [r["id"] for r in rest["rows"]] == [3, 4]


async def test_dump_inject_roundtrip(kernel, store):
    await _append(
        kernel,
        "alice",
        [{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}],
    )
    # a plain verb: whoever the gate admits may dump
    assert (await _call(kernel, "mem", {"type": "dump"}, as_="alice"))["tables"]
    tables = (await _call(kernel, "mem", {"type": "dump"}))["tables"]
    await _call(kernel, "mem", {"type": "prune", "before_id": 99}, as_="alice")
    assert (await _call(kernel, "mem", {"type": "read"}, as_="alice"))["rows"] == []
    out = await _call(kernel, "mem", {"type": "inject", "tables": tables})
    assert out == {"rows": 3}
    assert (await _call(kernel, "mem", {"type": "dump"}))["tables"] == tables
    # a broken FK is refused and nothing changes
    bad = {**tables, "fold_refs": [{**tables["fold_refs"][0], "members": [999]}]}
    assert (await _call(kernel, "mem", {"type": "inject", "tables": bad}))[
        "reason"
    ] == "bad_arg"
    assert (await _call(kernel, "mem", {"type": "dump"}))["tables"] == tables


async def test_a_superscript_digit_is_no_id_and_raises_nothing(kernel, store):
    """`'²'.isdigit()` is True and `int('²')` raises: a `read` naming it must not
    fail the whole call as a raised handler. It names no row, like any non-id
    (ASCII digits only)."""
    await _append(kernel, "alice", [{"id": 101, "role": "user", "content": "hi"}])
    out = await _call(kernel, "mem", {"type": "read", "ids": ["²", "101"]}, as_="alice")
    assert "error" not in out, out
    assert [r["id"] for r in out["rows"]] == [101], out


async def test_a_name_match_reads_the_rows_carrying_it(kernel, store):
    await _append(
        kernel,
        "alice",
        [
            {"id": 1, "kind": "call", "name": "job"},
            {"id": 2, "kind": "settle", "name": "job"},
            {"id": 3, "kind": "call", "name": "other"},
            {"id": 4, "kind": "call", "name": "job", "verb": "x"},
            {"id": 5, "kind": "clear"},
            {"id": 6, "kind": "call", "name": 7},
        ],
    )
    await _append(kernel, "bob", [{"id": 1, "kind": "call", "name": "job"}])

    async def ids(**q):
        out = await _call(kernel, "mem", {"type": "read", **q}, as_="alice")
        return [r["id"] for r in out["rows"]]

    assert await ids(match={"name": "job"}) == [1, 2, 4]
    assert await ids(kinds=["call"], match={"name": "job"}) == [1, 4]
    assert await ids(match={"name": "job", "verb": "x"}) == [4]
    assert await ids(match={"name": "job"}, order="desc", limit=1) == [4]
    assert await ids(match={"name": "job"}, from_last_kind="clear") == []
    assert await ids(match={"name": "nobody"}) == []
    assert await ids(match={"name": 7}) == [6]
    await _call(kernel, "mem", {"type": "prune", "before_id": 3}, as_="alice")
    assert await ids(match={"name": "job"}) == [4]
