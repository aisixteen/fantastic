"""`$subtree:<X>` (ingress) and HERITABLE `can_call` entries (egress).

Agents spawned DYNAMICALLY under one owner (a web leg's terminals and AIs) reach
a target without anybody naming each of them: the target's `callable_by` accepts
`"$subtree:<owner>"`, resolved against the LIVE tree at call time, and the owner
carries `can_call {<target>: {"verbs": [...], "for": "descendants"}}`, which
reaches for its descendants and never for the owner itself. The contract corpus
(`contract/agentic/specs/subtree_grants.yaml`) pins the same behaviour on every
runtime; these are the python units behind it plus the cases the corpus cannot
state (a foreign caller, the pure engine, persistence, web reachability).
"""

from __future__ import annotations

from kernel import sender_context
from kernel._authz import authorize_call, authorize_heritable

from _testkit import boot_root, persist

LIST_ALLOW = ["list", "allow", "allowed"]


def _tree(parent_of: dict[str, str | None]):
    """A resolver over a static parent map — `in_subtree(other, x)`."""

    def in_subtree(other: str, x: str) -> bool:
        node: str | None = other
        while node is not None:
            if node == x:
                return True
            node = parent_of.get(node)
        return False

    return in_subtree


# ─── the pure engine ────────────────────────────────────────────


def test_subtree_matches_x_and_every_descendant():
    t = _tree({"hub": "root", "kid": "hub", "grandkid": "kid", "other": "root"})
    g = {"$subtree:hub": ["list"]}
    for who in ("hub", "kid", "grandkid"):
        assert authorize_call(g, "list", who, holder_id="served", in_subtree=t).allowed
    assert not authorize_call(
        g, "list", "other", holder_id="served", in_subtree=t
    ).allowed


def test_a_foreign_caller_never_matches_subtree():
    # even when the resolver would say yes, a qualified `<guid>|<agent>` never matches
    g = {"$subtree:hub": "*"}
    assert not authorize_call(
        g, "list", "g1|hub", holder_id="served", in_subtree=lambda o, x: True
    ).allowed


def test_an_absent_subtree_root_matches_nothing():
    t = _tree({"kid": "hub"})
    g = {"$subtree:ghost": "*"}
    assert not authorize_call(
        g, "list", "kid", holder_id="served", in_subtree=t
    ).allowed
    # and with no resolver at all, the key matches nothing
    assert not authorize_call(
        {"$subtree:hub": "*"}, "list", "kid", holder_id="s"
    ).allowed


def test_subtree_is_structural_for_privileged_verbs():
    t = _tree({"kid": "hub"})
    # a NAMED privileged verb is covered …
    assert authorize_call(
        {"$subtree:hub": ["allow"]}, "allow", "kid", holder_id="s", in_subtree=t
    ).allowed
    # … `*` is not, where the privileged rule applies
    assert not authorize_call(
        {"$subtree:hub": "*"}, "allow", "kid", holder_id="s", in_subtree=t
    ).allowed
    # ingress (enforce_privileged=False) keeps `$children:*`-style acceptance
    assert authorize_call(
        {"$subtree:hub": "*"},
        "allow",
        "kid",
        holder_id="s",
        in_subtree=t,
        enforce_privileged=False,
    ).allowed


def test_a_heritable_entry_never_grants_its_holder():
    g = {"served": {"verbs": ["list"], "for": "descendants"}}
    assert not authorize_call(g, "list", "served", holder_id="hub").allowed


def test_authorize_heritable_named_and_wildcard():
    named = [{"served": {"verbs": ["list", "allow"], "for": "descendants"}}]
    assert authorize_heritable(named, "list", "served").allowed
    assert authorize_heritable(named, "allow", "served").allowed  # named privileged
    assert not authorize_heritable(named, "mkdir", "served").allowed
    assert not authorize_heritable(named, "list", "other").allowed
    wild = [{"served": {"verbs": ["*"], "for": "descendants"}}]
    assert authorize_heritable(wild, "list", "served").allowed
    assert not authorize_heritable(wild, "allow", "served").allowed  # `*` ≠ privileged
    # a plain entry in an ancestor is NOT heritable
    assert not authorize_heritable([{"served": ["list"]}], "list", "served").allowed


# ─── the live gate ──────────────────────────────────────────────


async def _create(k, **payload):
    out = (
        await k.send([{"target": k.id, "payload": {"type": "create_agent", **payload}}])
    )[0]["result"]
    return out


async def _as(k, sender, target, payload):
    with sender_context(sender):
        return (await k.send([{"target": target, "payload": payload}]))[0]["result"]


def _fb(**meta):
    return {
        "handler_module": "file_bridge.tools",
        "allow": ["."],
        "ingress_rule": "allow_all_ingress",
        **meta,
    }


async def _world(k):
    served = await _create(
        k, parent_id=k.id, id="served", **_fb(callable_by={"$subtree:hub": LIST_ALLOW})
    )
    assert served.get("id") == "served", served
    hub = await _create(
        k,
        parent_id=k.id,
        id="hub",
        **_fb(can_call={"served": {"verbs": LIST_ALLOW, "for": "descendants"}}),
    )
    assert hub.get("id") == "hub", hub
    kid = await _create(k, parent_id="hub", id="kid", **_fb())
    assert kid.get("id") == "kid", kid
    outsider = await _create(
        k, parent_id=k.id, id="outsider", **_fb(can_call={"served": ["list"]})
    )
    assert outsider.get("id") == "outsider", outsider


async def test_a_descendant_reaches_the_target_and_the_holder_does_not(seeded_kernel):
    k = seeded_kernel
    await _world(k)
    ok = await _as(k, "kid", "served", {"type": "list", "path": ""})
    assert "error" not in ok, ok
    held = await _as(k, "hub", "served", {"type": "list", "path": ""})
    assert (held.get("reason"), held.get("side")) == ("unauthorized", "egress"), held
    unnamed = await _as(k, "kid", "served", {"type": "mkdir", "path": "nope"})
    assert (unnamed.get("reason"), unnamed.get("side")) == ("unauthorized", "egress")
    priv = await _as(k, "kid", "served", {"type": "allow", "path": "p.txt"})
    assert priv.get("allowed") is True, priv


async def test_a_grandchild_spawned_later_is_covered(seeded_kernel):
    k = seeded_kernel
    await _world(k)
    gk = await _create(k, parent_id="kid", id="grandkid", **_fb())
    assert gk.get("id") == "grandkid", gk
    ok = await _as(k, "grandkid", "served", {"type": "list", "path": ""})
    assert "error" not in ok, ok


async def test_an_outsider_is_refused_on_ingress(seeded_kernel):
    k = seeded_kernel
    await _world(k)
    out = await _as(k, "outsider", "served", {"type": "list", "path": ""})
    assert (out.get("reason"), out.get("side")) == ("unauthorized", "ingress"), out


async def test_wildcard_heritable_never_covers_a_privileged_verb(seeded_kernel):
    k = seeded_kernel
    served = await _create(
        k,
        parent_id=k.id,
        id="served_w",
        **_fb(callable_by={"$subtree:hub_w": ["list", "allow"]}),
    )
    assert served.get("id") == "served_w", served
    await _create(
        k,
        parent_id=k.id,
        id="hub_w",
        **_fb(can_call={"served_w": {"verbs": ["*"], "for": "descendants"}}),
    )
    await _create(k, parent_id="hub_w", id="kid_w", **_fb())
    assert "error" not in await _as(
        k, "kid_w", "served_w", {"type": "list", "path": ""}
    )
    out = await _as(k, "kid_w", "served_w", {"type": "allow", "path": "x"})
    assert (out.get("reason"), out.get("side")) == ("unauthorized", "egress"), out


# ─── conferral ──────────────────────────────────────────────────


async def test_only_a_holder_confers_a_subset_of_a_heritable_entry(seeded_kernel):
    k = seeded_kernel
    await _world(k)
    heritable = {"served": {"verbs": ["list"], "for": "descendants"}}
    ok = await _as(
        k,
        "hub",
        k.id,
        {
            "type": "create_agent",
            "parent_id": "hub",
            "id": "hub_child",
            "handler_module": "file_bridge.tools",
            "can_call": heritable,
        },
    )
    assert ok.get("id") == "hub_child", ok
    benefits = await _as(
        k,
        "kid",
        k.id,
        {
            "type": "create_agent",
            "parent_id": "kid",
            "id": "kid_child",
            "handler_module": "file_bridge.tools",
            "can_call": heritable,
        },
    )
    assert (benefits.get("reason"), benefits.get("side")) == (
        "unauthorized",
        "delegation",
    ), benefits
    assert "cannot confer a heritable 'list' on served" in benefits["error"]
    more = await _as(
        k,
        "hub",
        k.id,
        {
            "type": "create_agent",
            "parent_id": "hub",
            "id": "hub_child2",
            "handler_module": "file_bridge.tools",
            "can_call": {
                "served": {"verbs": ["list", "disallow"], "for": "descendants"}
            },
        },
    )
    assert (more.get("reason"), more.get("side")) == ("unauthorized", "delegation")


# ─── write-time shape ───────────────────────────────────────────


async def test_bad_shapes_are_refused_at_write_time(seeded_kernel):
    k = seeded_kernel
    await _world(k)
    cases = [
        ("outsider", {"can_call": {"$subtree:hub": ["list"]}}, "callable_by-only"),
        ("served", {"callable_by": {"$subtree:": ["list"]}}, "LOCAL agent id"),
        ("served", {"callable_by": {"$subtree:g1|x": ["list"]}}, "LOCAL agent id"),
        (
            "hub",
            {"can_call": {"served": {"verbs": ["list"], "for": "siblings"}}},
            "must be exactly",
        ),
        (
            "hub",
            {"can_call": {"$parent": {"verbs": ["list"], "for": "descendants"}}},
            "never a token",
        ),
        (
            "served",
            {"callable_by": {"hub": {"verbs": ["list"], "for": "descendants"}}},
            "can_call-only",
        ),
        (
            "hub",
            {"can_call": {"served": {"verbs": [], "for": "descendants"}}},
            "must be exactly",
        ),
    ]
    for aid, patch, words in cases:
        r = (
            await k.send(
                [
                    {
                        "target": k.id,
                        "payload": {"type": "update_agent", "id": aid, **patch},
                    }
                ]
            )
        )[0]["result"]
        assert r.get("reason") == "invalid_meta", (patch, r)
        assert words in r["error"], (patch, r)


# ─── persistence + reflect ──────────────────────────────────────


async def test_both_forms_round_trip_through_disk(tmp_path, monkeypatch):
    from file_bridge import fs
    from kernel_state.tools import read_tree

    monkeypatch.chdir(tmp_path)
    root = boot_root()
    heritable = {"served": {"verbs": LIST_ALLOW, "for": "descendants"}}
    root.create("file_bridge.tools", id="hub", allow=["."], can_call=heritable)
    root.create(
        "file_bridge.tools",
        id="served",
        allow=["."],
        callable_by={"$subtree:hub": LIST_ALLOW},
    )
    persist(root)
    recs = {r["id"]: r for r in read_tree(fs.Root(tmp_path, ".fantastic"))}
    assert recs["hub"]["can_call"]["served"] == heritable["served"]
    assert recs["served"]["callable_by"]["$subtree:hub"] == LIST_ALLOW


async def test_reflect_shows_both_forms_verbatim(seeded_kernel):
    k = seeded_kernel
    await _world(k)
    r = (
        await k.send(
            [{"target": "kernel", "payload": {"type": "reflect", "detail": "full"}}]
        )
    )[0]["result"]
    nodes = {c["id"]: c for c in r["tree"]["children"]}
    assert nodes["served"]["callable_by"]["$subtree:hub"] == LIST_ALLOW
    assert nodes["hub"]["can_call"]["served"] == {
        "verbs": LIST_ALLOW,
        "for": "descendants",
    }


async def test_browser_reachability_honours_both_forms(seeded_kernel):
    from web.tools import _browser_reachable

    k = seeded_kernel
    await _create(
        k,
        parent_id=k.id,
        id="page",
        **_fb(can_call={"docs": {"verbs": ["read_stream"], "for": "descendants"}}),
    )
    await _create(
        k,
        parent_id=k.id,
        id="docs",
        **_fb(callable_by={"$subtree:page": ["read_stream"]}),
    )
    await _create(k, parent_id="page", id="page_web", **_fb())
    assert "docs" in _browser_reachable("page_web", k)
    # the owner itself holds only the heritable entry — not reachable from it
    assert "docs" not in _browser_reachable("page", k)
