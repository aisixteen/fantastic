"""Python twins of the Rust kernel `reflect` / `agent` unit tests (#170).

Each test is named `test_<rust test name>` and asserts what the Rust test asserts;
Python is the spec, so a failure here is a drift on the Python side too.
"""

from __future__ import annotations

import sys
import types

import pytest

from kernel import Agent, register_unlisted

MOD = "reflect_twins_probe.tools"
_mod = types.ModuleType(MOD)


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        if id == "boom":
            raise RuntimeError("reflect exploded")
        return {
            "id": id,
            "sentence": f"{id} is a roster probe",
            "handler_module": MOD,
            "mode": "watch",
        }
    return {"ok": True}


_mod.handler = _handler
_mod.VERBS = {}
register_unlisted(MOD, _mod)


async def _call(kernel, target: str, payload: dict) -> dict:
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def _full_call(kernel, target: str, payload: dict) -> dict:
    return (await kernel.send([{"target": target, "payload": payload}]))[0]


async def _mk(kernel, id: str, parent: str = "kernel_state", **meta) -> dict:
    r = await _call(
        kernel,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": parent,
            "handler_module": MOD,
            "id": id,
            **meta,
        },
    )
    assert r["id"] == id, f"fixture create failed: {r}"
    return r


async def test_child_reflect_has_no_guid(seeded_kernel):
    """guid is persistence-scoped and root-only: a child reflect never carries one."""
    await _mk(seeded_kernel, "kid_1")
    child = await _call(seeded_kernel, "kid_1", {"type": "reflect"})
    assert child["id"] == "kid_1"
    assert "guid" not in child, "guid must be root-only"
    root = await _call(seeded_kernel, "kernel", {"type": "reflect"})
    assert "guid" in root, "the root does carry it"


_DOC_KEYS = ["nested/../topic", "frontend/html-agent/../routing", "nested//child"]


async def test_readme_view_key_check_is_lexical_then_the_index(seeded_kernel):
    """A `.`/`..`/empty segment is a `bad key` whatever the filesystem holds; a
    well-formed key is a doc only if the index lists it."""
    k = seeded_kernel

    async def view(sel):
        r = await _call(k, "kernel", {"type": "reflect", "tree": "none", "readme": sel})
        return r["readme"]["error"]

    # `frontend` is a real tier in the doc tree, so the `..` check is lexical.
    idx = await _call(k, "kernel", {"type": "reflect", "tree": "none", "readme": "index"})
    keys = [e["key"] for e in idx["readme"]["index"]]
    assert "frontend" in keys
    for sel in _DOC_KEYS:
        assert await view(sel) == f"bad key {sel!r}"
    # A leading backslash becomes `/`: an empty first segment, the ORIGINAL echoed.
    assert await view("\\topic") == "bad key " + repr("\\topic")
    # `/` alone strips to an empty key.
    assert await view("/") == "bad key '/'"
    # Well formed but not in the index: `no doc`, the index is the second check.
    assert await view("nested/child") == "no doc 'nested/child'"
    assert await view("zzz") == "no doc 'zzz'"


async def test_an_agent_can_read_its_own_authority(seeded_kernel):
    """The gate is VISIBLE: an agent's own reflect carries both maps, and every
    wiring-tree node (`detail:full`) carries them too."""
    await _mk(
        seeded_kernel,
        "worker",
        can_call={"store": ["read"]},
        callable_by={"cli": "*", "$self": "*"},
    )
    own = await _call(seeded_kernel, "worker", {"type": "reflect"})
    assert own["can_call"]["store"] == ["read"]
    assert own["callable_by"]["cli"] == "*"
    tree = await _call(seeded_kernel, "kernel", {"type": "reflect", "detail": "full"})
    node = next(n for n in tree["tree"]["children"] if n["id"] == "worker")
    assert node["can_call"]["store"] == ["read"]
    assert node["callable_by"]["cli"] == "*"


async def test_tree_children_keep_creation_order(seeded_kernel):
    """The tree frame, `tree=ids` and a removal all read INSERTION order, never
    sorted by id."""
    k = seeded_kernel
    # Deliberately non-alphabetical: a sort would reorder these.
    for id in ("zulu", "mike", "alpha"):
        await _mk(k, id)
    await _mk(k, "gamma", parent="alpha")
    root = k.ctx.agents["kernel_state"]
    ours = ["zulu", "mike", "alpha"]
    ids = root.child_ids()
    assert [i for i in ids if i in ours] == ours

    full = await _call(k, "kernel", {"type": "reflect", "detail": "full"})
    kids = [c["id"] for c in full["tree"]["children"]]
    assert [i for i in kids if i in ours] == ours
    distilled = await _call(k, "kernel", {"type": "reflect"})
    kids = [c["id"] for c in distilled["tree"]["children"]]
    assert [i for i in kids if i in ours] == ours

    flat = (await _call(k, "kernel", {"type": "reflect", "tree": "ids"}))["tree"]
    assert flat[0] == "kernel_state"
    tail = flat[flat.index("zulu") :]
    assert tail == ["zulu", "mike", "alpha", "gamma"], flat

    # A removal leaves the survivors in place; it does not re-sort.
    out = await _call(k, "kernel", {"type": "delete_agent", "id": "mike"})
    assert out.get("deleted") is True, out
    ids = root.child_ids()
    assert [i for i in ids if i in ours] == ["zulu", "alpha"]


async def test_distilled_tree_reflect_echoes_to_node_watchers(seeded_kernel):
    """The per-node reflect is a REAL dispatch: the node's watchers hear
    `{type:'reflect'}` once, and an internal call row is written. `detail:full`
    dispatches nothing."""
    k = seeded_kernel
    await _mk(k, "watched")
    # A bare watcher (no handler): its own reflect is not dispatched, so the only
    # thing it can hear is the watched node's.
    Agent(id="tap", ctx=k.ctx, parent=k)
    inbox = k.ctx.ensure_inbox("tap")
    assert k.watch("watched", "tap") is None

    def about(agent_id):
        return [r for r in k.ctx.rows.values() if r.get("agent") == agent_id]

    await _call(k, "kernel", {"type": "reflect"})
    heard = []
    while not inbox.empty():
        heard.append(inbox.get_nowait())
    assert heard == [{"type": "reflect"}], "one echo per reflect"
    rows = about("watched")
    assert len(rows) == 1, rows
    assert rows[0]["kind"] == "call"
    assert rows[0]["verb"] == "reflect"
    assert rows[0]["internal"] is True
    assert about("tap") == [], "a bare node is not dispatched"

    await _call(k, "kernel", {"type": "reflect", "detail": "full"})
    assert inbox.empty()
    assert len(about("watched")) == 1


async def test_a_raising_roster_node_fails_the_reflect(seeded_kernel):
    """The exception propagates out of the roster: the whole reflect settles
    `failed`. The flat id index dispatches nothing, so it still answers."""
    k = seeded_kernel
    await _mk(k, "boom")
    out = await _full_call(k, "kernel", {"type": "reflect"})
    assert out["status"] == "failed", out
    assert out["result"] == {"error": "RuntimeError: reflect exploded", "raised": True}
    ids = await _full_call(k, "kernel", {"type": "reflect", "tree": "ids"})
    assert ids["status"] == "completed"
    assert "boom" in ids["result"]["tree"]


@pytest.mark.parametrize("flag", [None, True, False, 1, [], {}, "none", "ALL", ""])
async def test_a_non_string_tree_flag_omits_the_tree(seeded_kernel, flag):
    """Only the exact strings `all`/`ids` build a tree; absent means all."""
    absent = await _call(seeded_kernel, "kernel", {"type": "reflect"})
    assert isinstance(absent["tree"], dict), "absent => all"
    r = await _call(seeded_kernel, "kernel", {"type": "reflect", "tree": flag})
    assert "tree" not in r, f"tree={flag!r}: {r}"
    assert "next" not in r
    assert "more" not in r


async def test_bundles_page_like_python(seeded_kernel, monkeypatch):
    """`bundles=all|ids` page by name with the SAME `limit`/`after` as the tree
    index; `bundles_next` is the last name, only when more remain; a cursor naming
    no bundle gives an empty tail."""
    from kernel import _agent

    def ep(name):
        return types.SimpleNamespace(name=name, value=f"{name}.tools")

    monkeypatch.setattr(
        _agent,
        "entry_points",
        lambda group: [ep("charlie"), ep("alpha"), ep("bravo")],
    )
    for n in ("alpha", "bravo", "charlie"):
        monkeypatch.setitem(sys.modules, f"{n}.tools", types.ModuleType(f"{n}.tools"))
    k = seeded_kernel
    await _mk(k, "k1")
    await _mk(k, "k2")
    base = {"type": "reflect", "tree": "none"}

    all_ = await _call(k, "kernel", {**base, "bundles": "all"})
    assert all_["bundles"] == [
        {"name": "alpha", "handler_module": "alpha.tools"},
        {"name": "bravo", "handler_module": "bravo.tools"},
        {"name": "charlie", "handler_module": "charlie.tools"},
    ]
    assert "bundles_next" not in all_, "no further page => no key"

    ids = ["kernel_state", "cli", "k1", "k2"]
    k.ctx.agents["kernel_state"]  # the root is first of the flat index
    p1 = await _call(
        k, "kernel", {"type": "reflect", "tree": "ids", "bundles": "ids", "limit": 2}
    )
    assert p1["tree"] == ids[:2]
    assert p1["next"] == ids[1]
    assert p1["bundles"] == ["alpha", "bravo"]
    assert p1["bundles_next"] == "bravo"

    p2 = await _call(
        k,
        "kernel",
        {"type": "reflect", "tree": "ids", "bundles": "ids", "limit": 2, "after": ids[1]},
    )
    assert p2["tree"] == ids[2:]
    # The tree cursor names no bundle => the catalog cursor is gone => empty tail.
    assert p2["bundles"] == []
    assert "bundles_next" not in p2

    pa = await _call(k, "kernel", {**base, "bundles": "all", "limit": 1})
    assert pa["bundles"] == [{"name": "alpha", "handler_module": "alpha.tools"}]
    assert pa["bundles_next"] == "alpha"
    pb = await _call(
        k, "kernel", {**base, "bundles": "ids", "limit": 5, "after": "alpha"}
    )
    assert pb["bundles"] == ["bravo", "charlie"]
    assert "bundles_next" not in pb


async def test_empty_display_name_falls_back_to_id(seeded_kernel):
    """`display_name or id`: '' is no name in the reflect and the tree node; a
    real name stays."""
    k = seeded_kernel
    await _mk(k, "nameless", display_name="")
    await _mk(k, "named", display_name="Named")
    assert k.ctx.agents["nameless"]._reflect_identity()["display_name"] == "nameless"
    assert k.ctx.agents["named"]._reflect_identity()["display_name"] == "Named"
    tree = await _call(k, "kernel", {"type": "reflect", "detail": "full"})
    by_id = {n["id"]: n for n in tree["tree"]["children"]}
    assert by_id["nameless"]["display_name"] == "nameless"
    assert by_id["named"]["display_name"] == "Named"


async def test_name_or_id_treats_an_empty_display_name_as_none(seeded_kernel):
    """`display_name or id`: '' and a non-string are no name; None too."""
    k = seeded_kernel
    await _mk(k, "test_1", display_name="Testy")
    agent = k.ctx.agents["test_1"]
    assert (agent.display_name or agent.id) == "Testy"
    for v in ("", 5, None):
        await _call(k, "kernel", {"type": "update_agent", "id": "test_1", "display_name": v})
        assert (agent.display_name or agent.id) == "test_1", v
        assert agent._reflect_identity()["display_name"] == "test_1"
        snap = {r["agent_id"]: r for r in k.state_snapshot()}
        assert snap["test_1"]["name"] == "test_1"


@pytest.mark.parametrize(
    "value,locked",
    [
        (1, True),
        ("yes", True),
        (["x"], True),
        ({"a": 1}, True),
        (0, False),
        ("", False),
        ([], False),
        (False, False),
        (None, False),
    ],
)
async def test_delete_lock_is_python_truthiness(seeded_kernel, value, locked):
    """`delete_lock` locks by truthiness, not identity with True."""
    k = seeded_kernel
    await _mk(k, "victim", delete_lock=value)
    r = await _call(k, "kernel", {"type": "delete_agent", "id": "victim"})
    if locked:
        assert r["reason"] == "locked", r
        assert "victim" in k.ctx.agents
    else:
        assert r.get("deleted") is True, r
        assert "victim" not in k.ctx.agents
