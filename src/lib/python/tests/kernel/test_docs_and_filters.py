"""`docs` and the reflect subtree filters.

A reflect says what an agent IS; how to call it is the `docs` kernel verb,
answered by the root, ungated, in two tiers. A reflect's tree can be cut to the
branches a caller works on (`show_subtrees`) or around the ones it knows
(`ignore_subtrees`). The cross-runtime half lives in `contract/agentic/specs/docs.yaml`.
"""

from __future__ import annotations

from _testkit import as_agent
from kernel._docs import signature


async def _call(k, target, payload) -> dict:
    return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


async def _docs(k, **args) -> dict:
    return await _call(k, "kernel", {"type": "docs", **args})


# ─── the oneliner rule ─────────────────────────────────────────


def test_signature_cuts_at_the_first_top_level_period_or_dash():
    assert signature("args: a:int. Does a thing.") == "args: a:int"
    assert signature("args: a:int — does a thing") == "args: a:int"
    # never inside brackets: an arg note keeps its own dash and period
    assert (
        signature("args: q:str (req — e.g. a word), n:int? (default 5). Finds q.")
        == "args: q:str (req — e.g. a word), n:int? (default 5)"
    )
    # a wrapped first paragraph is joined; later paragraphs never count
    assert signature(
        "Start the loop unless `watch=false`\n(a loader). More.\n\nX."
    ) == ("Start the loop unless `watch=false` (a loader)")
    assert signature("No args.") == "No args"
    assert signature(None) == ""


# ─── docs ───────────────────────────────────────────────────────


async def test_reflect_carries_no_verb_docs(seeded_kernel):
    k = seeded_kernel
    k.create("exec_bridge.tools", id="runner", cwd=".")
    for target in ("kernel", "runner", "cli"):
        r = await _call(k, target, {"type": "reflect"})
        for key in ("verbs", "emits", "cancel", "more", "kernel_verbs", "wire"):
            assert key not in r, (target, key)
    full = await _call(k, "kernel", {"type": "reflect", "detail": "full"})
    assert all("verbs" not in n for n in full["tree"]["children"])


async def test_docs_oneliners_and_detailed(seeded_kernel):
    from exec_bridge.tools import EMITS, NOTES, VERBS

    k = seeded_kernel
    k.create("exec_bridge.tools", id="runner", cwd=".")
    one = await _docs(k, of="runner")
    assert list(one) == ["of", "bundle", "tier", "verbs"]
    assert (one["of"], one["bundle"], one["tier"]) == (
        "runner",
        "exec_bridge.tools",
        "oneliners",
    )
    assert list(one["verbs"]) == list(VERBS)
    assert one["verbs"]["status"] == "args: job_id:int?"
    full = await _docs(k, of="runner", tier="detailed")
    assert full["emits"] == EMITS and full["notes"] == NOTES
    assert len(full["verbs"]["start"]) > len(one["verbs"]["start"])
    only = await _docs(k, of="runner", tier="detailed", verb="start")
    assert list(only["verbs"]) == ["start"] and "emits" not in only


async def test_docs_of_a_bundle_and_of_the_kernel(seeded_kernel):
    k = seeded_kernel
    b = await _docs(k, of="scheduler.tools")
    assert b["bundle"] == "scheduler.tools" and "schedule" in b["verbs"]
    kv = await _docs(k, of="kernel")
    assert kv["bundle"] == "kernel" and "docs" in kv["verbs"]
    assert kv["verbs"]["cancel"] == "args: task_id:str"
    # the AI backends serve their shared EMITS / CANCEL / NOTES
    ai = await _docs(k, of="vllm_backend.tools", tier="detailed")
    assert "status" in ai["emits"] and "cancel" in ai["cancel"]


async def test_docs_of_an_agent_read_its_own_table_when_its_bundle_names_one(
    seeded_kernel,
):
    """PER-INSTANCE VERBS: a bundle whose agents serve different
    verbs names each one's table (`instance_verbs(agent)`); None keeps the bundle's."""
    import types

    from kernel import register_unlisted

    mod = types.ModuleType("per_instance_probe")

    async def _ping(id, payload, kernel):
        """No args. The bundle's own verb."""

    mod.VERBS = {"ping": _ping}
    mod.handler = lambda id, payload, kernel: None
    mod.instance_verbs = lambda agent: (
        {"ping": "No args. Still here.", "own_" + agent.id: "args: x:int. Only mine."}
        if agent.id != "plain"
        else None
    )
    register_unlisted("per_instance_probe", mod)
    k = seeded_kernel
    for aid in ("a1", "a2", "plain"):
        k.create("per_instance_probe", id=aid)
    a1 = await _docs(k, of="a1")
    a2 = await _docs(k, of="a2", tier="detailed", verb="own_a2")
    assert a1["verbs"] == {"ping": "No args", "own_a1": "args: x:int"}, a1
    assert a2["verbs"] == {"own_a2": "args: x:int. Only mine."}, a2
    assert (await _docs(k, of="plain"))["verbs"] == {"ping": "No args"}
    gone = await _docs(k, of="a1", verb="own_a2")
    assert gone["reason"] == "bad_arg" and "own_a1" in gone["hint"], gone


async def test_docs_refusals_are_coded(seeded_kernel):
    k = seeded_kernel
    assert (await _docs(k))["reason"] == "missing_arg"
    for args in (
        {"of": "nothing_here"},
        {"of": "kernel", "verb": "nope"},
        {"of": "kernel", "tier": "everything"},
    ):
        r = await _docs(k, **args)
        assert r["reason"] == "bad_arg", (args, r)


async def test_docs_is_gated_like_any_kernel_verb(seeded_kernel):
    """No verb is exempt by name: `docs` takes the two-sided
    check against the root — an agent without the grant is refused at egress; one
    granted `docs` (both halves) reads them, and that grant covers nothing else."""
    k = seeded_kernel
    k.create(None, id="nobody")
    k.create(None, id="reader", can_call={"kernel_state": ["docs"]})
    root = k.ctx.agents["kernel_state"]
    root._meta["callable_by"] = {
        **(root._meta.get("callable_by") or {}),
        "reader": ["docs"],
    }
    with as_agent(k, "nobody"):
        refused = await _docs(k, of="kernel", verb="ps")
    with as_agent(k, "reader"):
        d = await _docs(k, of="kernel", verb="ps")
        refl = await _call(k, "kernel", {"type": "reflect"})
    assert refused["reason"] == "unauthorized" and refused["side"] == "egress", refused
    assert d["verbs"] == {"ps": "No args"}
    assert refl["reason"] == "unauthorized" and refl["side"] == "egress"


# ─── subtree filters ────────────────────────────────────────────


def _tree(k):
    k.create(None, id="a")
    a = k.ctx.agents["a"]
    a.create(None, id="a1")
    a.create(None, id="a2")
    k.ctx.agents["a2"].create(None, id="a21")
    k.create(None, id="b")
    k.ctx.agents["b"].create(None, id="b1")


async def test_ignore_prunes_a_branch_to_a_counted_stub(seeded_kernel):
    k = seeded_kernel
    _tree(k)
    r = await _call(k, "kernel", {"type": "reflect", "ignore_subtrees": ["a"]})
    kids = {c["id"]: c for c in r["tree"]["children"]}
    assert kids["a"] == {"id": "a", "pruned": 4}
    assert [c["id"] for c in kids["b"]["children"]] == ["b1"]
    ids = (
        await _call(
            k, "kernel", {"type": "reflect", "tree": "ids", "ignore_subtrees": ["a2"]}
        )
    )["tree"]
    assert {"id": "a2", "pruned": 2} in ids and "a21" not in ids and "a1" in ids


async def test_show_answers_only_the_named_branches(seeded_kernel):
    k = seeded_kernel
    _tree(k)
    r = await _call(k, "kernel", {"type": "reflect", "show_subtrees": ["a2", "b"]})
    assert [n["id"] for n in r["tree"]] == ["a2", "b"]
    assert r["tree"][0]["children"][0]["id"] == "a21"
    # a shown id under another shown id is already inside that branch
    r = await _call(k, "kernel", {"type": "reflect", "show_subtrees": ["a", "a21"]})
    assert [n["id"] for n in r["tree"]] == ["a"]
    full = await _call(
        k, "kernel", {"type": "reflect", "detail": "full", "show_subtrees": ["b"]}
    )
    assert full["tree"][0]["id"] == "b" and full["tree"][0]["children"][0]["id"] == "b1"


async def test_show_then_ignore_inside_it(seeded_kernel):
    k = seeded_kernel
    _tree(k)
    ids = (
        await _call(
            k,
            "kernel",
            {
                "type": "reflect",
                "tree": "ids",
                "show_subtrees": ["a"],
                "ignore_subtrees": ["a2"],
            },
        )
    )["tree"]
    assert ids == ["a", "a1", {"id": "a2", "pruned": 2}]


async def test_a_filter_naming_no_agent_in_the_tree_is_refused(seeded_kernel):
    k = seeded_kernel
    _tree(k)
    for arg in ("show_subtrees", "ignore_subtrees"):
        r = await _call(k, "kernel", {"type": "reflect", arg: ["ghost"]})
        assert r["reason"] == "bad_arg" and r["arg"] == arg, r
        r = await _call(k, "kernel", {"type": "reflect", arg: "a"})
        assert r["reason"] == "bad_arg", r
    # an agent outside the REFLECTED agent's subtree is not in its tree
    r = await _call(k, "a", {"type": "reflect", "show_subtrees": ["b1"]})
    assert r["reason"] == "bad_arg", r
