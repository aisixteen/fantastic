"""`create_agent` is gated on the PLACEMENT TARGET, and `$loader` is real reach.

Both measured 2026-09-02 against the live kernel:

- An agent holding the ordinary "may author tools" wiring (`<root>:[create_agent]`)
  could plant a child under ANY agent — `parent_id: "exec_bridge"` — and the child's
  stamped `$parent:*` baseline handed it unrestricted reach into that agent. The
  gate was asking about the ROOT (which merely ANSWERS the verb) instead of the
  parent (which is ACTED UPON).
- `_check_conferral` skipped every `$`-prefixed key as "a structural token that
  confers no lateral reach", but `$loader` resolves to the ROOT for whoever holds
  it. A grandchild refused `ps` on the root created a child with
  `can_call:{"$loader":"*"}` and that child read the task table, the ledger, and
  could have called `shutdown_kernel`.
"""

from __future__ import annotations

from kernel import sender_context


async def _mk(kernel, agent_id, parent=None, **meta):
    payload = {
        "type": "create_agent",
        "parent_id": parent or kernel.id,
        "id": agent_id,
        "handler_module": "file_bridge.tools",
        "allow": ["."],
        "ingress_rule": "allow_all_ingress",
        **meta,
    }
    rec = (await kernel.send([{"target": kernel.id, "payload": payload}]))[0]["result"]
    assert rec.get("id") == agent_id, rec
    return agent_id


async def _create_as(kernel, sender, **payload):
    with sender_context(sender):
        return (
            await kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                            **payload,
                        },
                    }
                ]
            )
        )[0]["result"]


async def test_an_agent_creates_under_itself_with_no_grant(seeded_kernel):
    """Self-extension is baseline at ANY depth — the old exemption only fired for
    a direct child of the root, because it keyed on the root grant."""
    mid = await _mk(seeded_kernel, "pg_mid")
    deep = await _mk(seeded_kernel, "pg_deep", parent=mid)  # a GRANDCHILD
    r = await _create_as(seeded_kernel, deep, parent_id=deep, id="pg_kid")
    assert r.get("id") == "pg_kid", r


async def test_creating_under_own_descendant_is_baseline(seeded_kernel):
    mid = await _mk(seeded_kernel, "pg2_mid")
    kid = await _mk(seeded_kernel, "pg2_kid", parent=mid)
    r = await _create_as(seeded_kernel, mid, parent_id=kid, id="pg2_grandkid")
    assert r.get("id") == "pg2_grandkid", r


async def test_a_root_create_grant_does_not_reach_a_sibling(seeded_kernel):
    """THE ESCALATION. `<root>:[create_agent]` is the ordinary authoring grant;
    it must not be a key to every other agent's subtree."""
    victim = await _mk(seeded_kernel, "pg3_victim")
    author = await _mk(
        seeded_kernel,
        "pg3_author",
        can_call={
            "$self": "*",
            "$parent": "*",
            "$children": "*",
            seeded_kernel.id: ["*", "create_agent", "update_agent"],
        },
    )
    r = await _create_as(seeded_kernel, author, parent_id=victim, id="pg3_mole")
    assert r.get("reason") == "unauthorized", r
    assert r.get("side") == "egress", r
    assert victim in r["error"] and "create under" in r["error"], r
    assert seeded_kernel.ctx.agents.get("pg3_mole") is None


async def test_creating_under_an_unrelated_agent_needs_both_halves(seeded_kernel):
    victim = await _mk(seeded_kernel, "pg4_victim")
    author = await _mk(
        seeded_kernel,
        "pg4_author",
        can_call={
            "$self": "*",
            "$parent": "*",
            "$children": "*",
            victim: ["create_agent"],
        },
    )
    # Egress granted, ingress not yet: the parent must accept a child.
    r = await _create_as(seeded_kernel, author, parent_id=victim, id="pg4_a")
    assert r.get("reason") == "unauthorized" and r.get("side") == "ingress", r
    # Open the parent's ingress → the edge exists.
    await seeded_kernel.send(
        [
            {
                "target": seeded_kernel.id,
                "payload": {
                    "type": "update_agent",
                    "id": victim,
                    "callable_by": {
                        "$self": "*",
                        "$parent": "*",
                        "$children": "*",
                        author: ["create_agent"],
                    },
                },
            }
        ]
    )
    ok = await _create_as(seeded_kernel, author, parent_id=victim, id="pg4_b")
    assert ok.get("id") == "pg4_b", ok


async def test_loader_token_in_a_conferred_grant_is_bounded(seeded_kernel):
    """`$loader` is not structural — it resolves to the root for ANY holder, so
    conferring it is conferring reach on the kernel's control surface."""
    mid = await _mk(seeded_kernel, "pg5_mid")
    deep = await _mk(
        seeded_kernel,
        "pg5_deep",
        parent=mid,
        can_call={
            "$self": "*",
            "$parent": "*",
            "$children": "*",
            seeded_kernel.id: ["create_agent"],
        },
    )
    # The granter itself cannot reach the root's task table…
    with sender_context(deep):
        denied = (
            await seeded_kernel.send([{"target": "kernel", "payload": {"type": "ps"}}])
        )[0]["result"]
    assert denied.get("reason") == "unauthorized", denied
    # …so it cannot hand that reach to a child either.
    r = await _create_as(
        seeded_kernel,
        deep,
        parent_id=deep,
        id="pg5_kid",
        can_call={"$loader": "*"},
    )
    assert r.get("reason") == "unauthorized" and r.get("side") == "delegation", r
    assert "$loader" in r["error"], r
    assert seeded_kernel.ctx.agents.get("pg5_kid") is None


async def test_a_granter_that_holds_the_root_may_confer_loader(seeded_kernel):
    holder = await _mk(
        seeded_kernel,
        "pg6_holder",
        can_call={
            "$self": "*",
            "$parent": "*",
            "$children": "*",
            seeded_kernel.id: "*",
        },
    )
    r = await _create_as(
        seeded_kernel,
        holder,
        parent_id=holder,
        id="pg6_kid",
        can_call={"$loader": "*"},
    )
    assert r.get("id") == "pg6_kid", r


async def test_an_unknown_structural_token_is_refused(seeded_kernel):
    a = await _mk(seeded_kernel, "pg7_a")
    r = await _create_as(
        seeded_kernel, a, parent_id=a, id="pg7_kid", can_call={"$everything": "*"}
    )
    assert r.get("reason") == "bad_arg" and r.get("side") == "delegation", r
    assert "$everything" in r["error"], r
