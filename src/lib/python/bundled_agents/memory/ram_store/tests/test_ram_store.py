"""ram_store: tenancy by caller, grants + FK cascade, search, inject/dump.

Callers act through `sender_context` (a real agent send, gated both ways); the
operator is a bare `kernel.send`. The root's guid is set by hand — the root is not
booted in these tests, and the store keys every row by it."""

from __future__ import annotations

import pytest

from kernel import remote_caller_context, sender_context


async def _call(kernel, target, payload, *, as_=None):
    if as_ is None:
        return (await kernel.send([{"target": target, "payload": payload}]))[0][
            "result"
        ]
    with sender_context(as_):
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
        operator=True,
        admins=["kernel"],
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
    out = await _call(
        kernel,
        "mem",
        {"type": "append", "owner": "bob", "rows": [{"id": 5}]},
        as_="alice",
    )
    assert out["reason"] == "unauthorized"
    out = await _call(kernel, "mem", {"type": "read", "owner": "bob"}, as_="alice")
    assert out["reason"] == "unauthorized"


async def test_missing_id_takes_the_kernels_next(kernel, store):
    before = kernel.ctx.next_id
    out = await _append(kernel, "alice", [{"content": "x"}])
    assert out["ids"][0] >= before


async def test_operator_needs_explicit_service(kernel, store):
    kernel.ctx.update("mem", operator=False)
    out = await _call(kernel, "mem", {"type": "read"})
    assert out["reason"] == "unauthorized"
    kernel.ctx.update("mem", operator=True)
    await _call(kernel, "mem", {"type": "append", "rows": [{"id": 7, "kind": "call"}]})
    got = await _call(kernel, "mem", {"type": "read", "kinds": ["call"]})
    assert [r["id"] for r in got["rows"]] == [7]
    # the operator's rows are its own key — alice sees none of them
    assert (await _call(kernel, "mem", {"type": "read"}, as_="alice"))["rows"] == []


async def test_admin_may_name_an_owner(kernel, store):
    await _append(kernel, "alice", [{"id": 1, "content": "a"}])
    got = await _call(kernel, "mem", {"type": "read", "owner": "alice"})
    assert [r["id"] for r in got["rows"]] == [1]
    out = await _call(
        kernel, "mem", {"type": "append", "owner": "alice", "rows": [{"id": 2}]}
    )
    assert out["ids"] == [2]


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


async def test_share_read_and_zero_tolerance(kernel, store):
    await _append(kernel, "alice", [{"id": i, "content": f"a{i}"} for i in (1, 2, 3)])
    await _append(kernel, "bob", [{"id": 50, "content": "b"}])
    g = await _call(
        kernel,
        "mem",
        {"type": "share", "with": "bob", "scope": {"head_until": 2}, "why": "context"},
        as_="alice",
    )
    assert g["created"] and g["anchor"] == 1
    got = await _call(kernel, "mem", {"type": "read", "grant": g["grant"]}, as_="bob")
    assert [r["id"] for r in got["rows"]] == [1, 2]
    # carol holds no grant
    out = await _call(kernel, "mem", {"type": "read", "grant": g["grant"]}, as_="carol")
    assert out["reason"] == "unauthorized"
    # bob cannot share alice's rows — not under his key
    out = await _call(
        kernel,
        "mem",
        {"type": "share", "with": "carol", "scope": {"row": 1}},
        as_="bob",
    )
    assert out["reason"] == "not_found"
    # an identical share is the same grant
    again = await _call(
        kernel,
        "mem",
        {"type": "share", "with": "bob", "scope": {"head_until": 2}},
        as_="alice",
    )
    assert again["grant"] == g["grant"] and not again["created"]


async def test_frozen_stops_live_follows(kernel, store):
    await _append(kernel, "alice", [{"id": 1}, {"id": 2}])
    frozen = await _call(
        kernel,
        "mem",
        {"type": "share", "with": "bob", "scope": {"head_until": 2}},
        as_="alice",
    )
    live = await _call(
        kernel,
        "mem",
        {"type": "share", "with": "bob", "scope": {"head_until": 2}, "mode": "live"},
        as_="alice",
    )
    await _append(kernel, "alice", [{"id": 3}])
    f = await _call(
        kernel, "mem", {"type": "read", "grant": frozen["grant"]}, as_="bob"
    )
    lv = await _call(kernel, "mem", {"type": "read", "grant": live["grant"]}, as_="bob")
    assert [r["id"] for r in f["rows"]] == [1, 2]
    assert [r["id"] for r in lv["rows"]] == [1, 2, 3]


async def test_fold_scope_reaches_members(kernel, store):
    await _append(
        kernel,
        "alice",
        [{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}, {"id": 4}],
    )
    g = await _call(
        kernel,
        "mem",
        {"type": "share", "with": "bob", "scope": {"fold": 3}},
        as_="alice",
    )
    got = await _call(kernel, "mem", {"type": "read", "grant": g["grant"]}, as_="bob")
    assert [r["id"] for r in got["rows"]] == [1, 2, 3]
    assert got["rows"][2]["refs"] == [1, 2]


async def test_prune_cascades_to_grants_and_fold_refs(kernel, store):
    await _append(
        kernel,
        "alice",
        [{"id": 1}, {"id": 2}, {"id": 3, "kind": "fold", "refs": [1, 2]}],
    )
    g = await _call(
        kernel,
        "mem",
        {"type": "share", "with": "bob", "scope": {"head_until": 3}},
        as_="alice",
    )
    out = await _call(kernel, "mem", {"type": "prune", "before_id": 2}, as_="alice")
    assert out == {"pruned": 1, "grants_removed": 1}
    gone = await _call(kernel, "mem", {"type": "read", "grant": g["grant"]}, as_="bob")
    assert gone["reason"] == "not_found"
    fold = await _call(kernel, "mem", {"type": "read", "ids": [3]}, as_="alice")
    assert fold["rows"][0]["refs"] == [2]
    listed = await _call(
        kernel, "mem", {"type": "grants", "side": "given"}, as_="alice"
    )
    assert listed["grants"] == []


async def test_unshare_and_grants_listing(kernel, store):
    await _append(kernel, "alice", [{"id": 1}])
    g = await _call(
        kernel,
        "mem",
        {"type": "share", "with": "bob", "scope": {"row": 1}},
        as_="alice",
    )
    rec = await _call(kernel, "mem", {"type": "grants", "side": "received"}, as_="bob")
    assert [x["owner"] for x in rec["grants"]] == ["alice"]
    out = await _call(
        kernel, "mem", {"type": "unshare", "grant": g["grant"]}, as_="bob"
    )
    assert out["reason"] == "unauthorized"
    assert (
        await _call(
            kernel, "mem", {"type": "unshare", "grant": g["grant"]}, as_="alice"
        )
    )["unshared"]
    out = await _call(kernel, "mem", {"type": "read", "grant": g["grant"]}, as_="bob")
    assert out["reason"] == "not_found"


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
    await _call(
        kernel,
        "mem",
        {"type": "share", "with": "bob", "scope": {"fold": 3}},
        as_="alice",
    )
    # only admins
    assert (await _call(kernel, "mem", {"type": "dump"}, as_="alice"))[
        "reason"
    ] == "unauthorized"
    tables = (await _call(kernel, "mem", {"type": "dump"}))["tables"]
    await _call(kernel, "mem", {"type": "prune", "before_id": 99}, as_="alice")
    assert (await _call(kernel, "mem", {"type": "read"}, as_="alice"))["rows"] == []
    out = await _call(kernel, "mem", {"type": "inject", "tables": tables})
    assert out == {"rows": 3, "grants": 1}
    assert (await _call(kernel, "mem", {"type": "dump"}))["tables"] == tables
    # a broken FK is refused and nothing changes
    bad = {**tables, "grants": [{**tables["grants"][0], "anchor": 999}]}
    assert (await _call(kernel, "mem", {"type": "inject", "tables": bad}))[
        "reason"
    ] == "bad_arg"
    assert (await _call(kernel, "mem", {"type": "dump"}))["tables"] == tables


async def test_remote_caller_is_keyed_by_its_qualified_address(kernel, store):
    # a relayed call is gated AS the foreign caller: the store names it
    cb = dict(kernel.ctx.agents["mem"]._meta["callable_by"])
    kernel.ctx.update("mem", callable_by={**cb, "g2|scout": "*"})
    with remote_caller_context("leg", "g2|scout"):
        out = await _call(
            kernel, "mem", {"type": "append", "rows": [{"id": 1}]}, as_="leg"
        )
    assert out["ids"] == [1]
    tables = (await _call(kernel, "mem", {"type": "dump"}))["tables"]
    assert [r["owner"] for r in tables["rows"]] == ["g2|scout"]
    # the leg's own writes stay the leg's
    await _call(kernel, "mem", {"type": "append", "rows": [{"id": 2}]}, as_="leg")
    owners = sorted(
        r["owner"]
        for r in (await _call(kernel, "mem", {"type": "dump"}))["tables"]["rows"]
    )
    assert owners == ["g1|leg", "g2|scout"]


async def test_a_superscript_digit_is_no_id_and_raises_nothing(kernel, store):
    """`'²'.isdigit()` is True and `int('²')` raises: a `read` naming it used to
    fail the whole call as a raised handler. It names no row, like any non-id
    (parity 2026-09-30 — rust/swift accept ASCII digits only)."""
    await _append(kernel, "alice", [{"id": 101, "role": "user", "content": "hi"}])
    out = await _call(kernel, "mem", {"type": "read", "ids": ["²", "101"]}, as_="alice")
    assert "error" not in out, out
    assert [r["id"] for r in out["rows"]] == [101], out


# ─── grant scopes, pinned (reduction candidate K: one scope-coverage rule) ───
#
# ai_core decides row-event bodies with its own copy of these semantics; sharing
# one function must keep what a grant READS here, per scope kind and mode.


async def _alice_tree(kernel):
    rows = [{"id": i, "content": f"r{i}"} for i in (1, 2, 3, 4, 5)]
    rows += [
        {"id": 6, "kind": "fold", "refs": [2, 3], "content": "f6"},
        {"id": 7, "kind": "fold", "refs": [6, 4], "content": "f7"},
    ]
    assert (await _append(kernel, "alice", rows))["appended"] == 7


async def _granted(kernel, scope, mode="frozen") -> list:
    g = await _call(
        kernel,
        "mem",
        {"type": "share", "with": "bob", "scope": scope, "mode": mode},
        as_="alice",
    )
    assert "grant" in g, g
    r = await _call(kernel, "mem", {"type": "read", "grant": g["grant"]}, as_="bob")
    return g, [row["id"] for row in r["rows"]]


async def test_each_scope_kind_reads_exactly_its_rows(kernel, store):
    await _alice_tree(kernel)
    assert (await _granted(kernel, {"row": 3}))[1] == [3]
    assert (await _granted(kernel, {"range": {"from": 2, "to": 4}}))[1] == [2, 3, 4]
    assert (await _granted(kernel, {"head_until": 3}))[1] == [1, 2, 3]
    # a fold reaches its whole closure: itself, its members, a member fold's members
    assert (await _granted(kernel, {"fold": 7}))[1] == [2, 3, 4, 6, 7]


async def test_live_grants_follow_the_head_and_frozen_ones_do_not(kernel, store):
    await _alice_tree(kernel)
    frozen_range, _ = await _granted(kernel, {"range": {"from": 2, "to": 4}})
    live_range, _ = await _granted(kernel, {"range": {"from": 2, "to": 4}}, "live")
    frozen_head, _ = await _granted(kernel, {"head_until": 5})
    live_head, _ = await _granted(kernel, {"head_until": 5}, "live")
    assert frozen_head["as_of"] == 7 and live_head["as_of"] is None
    await _append(kernel, "alice", [{"id": 8, "content": "r8"}])

    async def read(g):
        r = await _call(kernel, "mem", {"type": "read", "grant": g["grant"]}, as_="bob")
        return [row["id"] for row in r["rows"]]

    assert await read(frozen_range) == [2, 3, 4]
    assert await read(live_range) == [2, 3, 4, 5, 6, 7, 8]  # live ignores `to`
    assert await read(frozen_head) == [1, 2, 3, 4, 5]
    assert await read(live_head) == [1, 2, 3, 4, 5, 6, 7, 8]


@pytest.mark.parametrize(
    "scope, reason, error",
    [
        (
            [1, 2],
            "bad_arg",
            "share: `scope` must be ONE of {head_until: <row>}, {range: {from, to}}, "
            "{fold: <id>}, {row: <id>} — a descriptor, never a list of messages",
        ),
        ({"range": [1, 2]}, "bad_arg", "share: range must be {from, to}"),
        (
            {"range": {"from": 4, "to": 2}},
            "bad_arg",
            "share: range needs from <= to (row ids)",
        ),
        (
            {"range": {"from": 50, "to": 60}},
            "not_found",
            "share: none of YOUR rows lie in 50..60",
        ),
        ({"row": "x"}, "bad_arg", "share: row must be a row id"),
        ({"head_until": 0}, "bad_arg", "share: head_until must be a row id"),
        (
            {"row": 99},
            "not_found",
            "share: row 99 is not one of YOUR rows — only rows you own can be shared",
        ),
        ({"fold": 3}, "bad_arg", "share: row 3 is not a fold"),
    ],
)
async def test_scope_refusal_texts(kernel, store, scope, reason, error):
    await _alice_tree(kernel)
    r = await _call(
        kernel, "mem", {"type": "share", "with": "bob", "scope": scope}, as_="alice"
    )
    assert r == {"error": error, "reason": reason, "arg": "scope"}


async def test_share_argument_refusals(kernel, store):
    await _alice_tree(kernel)

    async def share(**kw):
        return await _call(
            kernel, "mem", {"type": "share", "scope": {"row": 1}, **kw}, as_="alice"
        )

    assert await share() == {
        "error": "share: with required",
        "reason": "missing_arg",
        "arg": "with",
    }
    assert await share(**{"with": "alice"}) == {
        "error": "share: you already read your own rows",
        "reason": "bad_arg",
        "arg": "with",
    }
    r = await share(**{"with": "bob", "mode": "forever"})
    assert r["reason"] == "bad_arg" and r["arg"] == "mode", r
    assert r["error"].startswith("share: mode must be one of ")
    # carol has no rows up to 3
    r = await _call(
        kernel,
        "mem",
        {"type": "share", "with": "bob", "scope": {"head_until": 3}},
        as_="carol",
    )
    assert r == {
        "error": "share: you have no rows up to 3",
        "reason": "not_found",
        "arg": "scope",
    }
