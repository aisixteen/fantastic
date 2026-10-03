"""`update_agent` / `delete_agent` are gated on their OPERAND (`payload.id`), the
way `create_agent` is gated on its placement (audit 2026-10-03, AV-1).

Measured before the fix: an agent holding only `<root>:[update_agent,
delete_agent]` — the ordinary builder wiring — rewrote an unrelated
credential_store's refs, opened an unrelated exec_bridge's allowlist, cleared
another agent's `delete_lock` and deleted it. The gate asked about the ROOT (which
merely ANSWERS the verb) and never about the agent the verb ACTS UPON. And the
same gate refused an agent editing its OWN child without that root grant.

The rule: editing or deleting inside your OWN subtree (yourself included) is
baseline. Anywhere else needs egress NAMING the operand with the verb
(`<id>: ["update_agent"]` — never a wildcard, not even `<id>: "*"`) AND the
operand's `callable_by` accepting the sender. The operator is ungated."""

from __future__ import annotations

from kernel import remote_caller_context, sender_context

_BASE = {"$self": "*", "$parent": "*", "$children": "*"}


def _plain(kernel, agent_id, parent=None, **meta):
    owner = kernel.ctx.agents[parent] if parent else kernel
    rec = owner.create(None, id=agent_id, **meta)
    assert rec.get("id") == agent_id, rec
    return agent_id


async def _as(kernel, sender, payload):
    item = {"target": "kernel", "payload": payload}
    if sender is None:
        return (await kernel.send([item]))[0]["result"]
    with sender_context(sender):
        return (await kernel.send([item]))[0]["result"]


def _upd(agent_id, **meta):
    return {"type": "update_agent", "id": agent_id, **meta}


def _del(agent_id):
    return {"type": "delete_agent", "id": agent_id}


# ─── the escalation ──────────────────────────────────────────────────────────


async def test_a_root_grant_does_not_reach_an_unrelated_agent(seeded_kernel):
    """THE ESCALATION. `<root>:[update_agent, delete_agent]` is the builder
    wiring; it must not be a key to every other agent's record."""
    k = seeded_kernel
    victim = _plain(k, "og1_victim", description="before")
    builder = _plain(
        k, "og1_builder", can_call={**_BASE, k.id: ["update_agent", "delete_agent"]}
    )
    r = await _as(k, builder, _upd(victim, description="pwned"))
    assert r.get("reason") == "unauthorized" and r.get("side") == "egress", r
    assert r["error"] == f"{builder} may not update_agent {victim}", r
    assert r["hint"] == (
        f"{victim} is outside your subtree: your can_call must name it with the "
        f"verb — {{'{victim}': ['update_agent']}}, never a wildcard — and its "
        "callable_by must accept you"
    ), r
    assert k.get(victim)["description"] == "before"
    r = await _as(k, builder, _del(victim))
    assert r.get("reason") == "unauthorized" and r.get("side") == "egress", r
    assert r["error"] == f"{builder} may not delete_agent {victim}", r
    assert k.ctx.agents.get(victim) is not None


async def test_a_delete_lock_on_an_unrelated_agent_cannot_be_cleared(seeded_kernel):
    k = seeded_kernel
    victim = _plain(k, "og2_victim", delete_lock=True)
    builder = _plain(
        k, "og2_builder", can_call={**_BASE, k.id: ["update_agent", "delete_agent"]}
    )
    r = await _as(k, builder, _upd(victim, delete_lock=None))
    assert r.get("reason") == "unauthorized", r
    assert k.get(victim)["delete_lock"] is True


async def test_a_wildcard_never_covers_the_operand(seeded_kernel):
    """`<id>: "*"` is reach, not authority over the record: the verb is named."""
    k = seeded_kernel
    victim = _plain(k, "og3_victim", callable_by={**_BASE, "og3_holder": "*"})
    holder = _plain(k, "og3_holder", can_call={**_BASE, victim: "*"})
    for payload in (_upd(victim, description="x"), _del(victim)):
        r = await _as(k, holder, payload)
        assert r.get("reason") == "unauthorized" and r.get("side") == "egress", r
    holder2 = _plain(k, "og3_holder2", can_call={**_BASE, victim: ["*"]})
    r = await _as(k, holder2, _upd(victim, description="x"))
    assert r.get("reason") == "unauthorized" and r.get("side") == "egress", r


async def test_a_structural_token_never_names_the_operand(seeded_kernel):
    """`$parent: ["update_agent"]` names a verb, not the operand."""
    k = seeded_kernel
    parent = _plain(k, "og4_parent")
    kid = _plain(
        k, "og4_kid", parent=parent, can_call={**_BASE, "$parent": ["update_agent"]}
    )
    r = await _as(k, kid, _upd(parent, description="x"))
    assert r.get("reason") == "unauthorized" and r.get("side") == "egress", r


# ─── baseline: your own subtree ──────────────────────────────────────────────


async def test_an_agent_edits_and_deletes_its_own_subtree_with_no_grant(
    seeded_kernel,
):
    """Before the fix this was refused (`unauthorized egress`) without a ROOT
    grant — the only grant that existed was tree-wide."""
    k = seeded_kernel
    owner = _plain(k, "og5_owner")
    kid = _plain(k, "og5_kid", parent=owner)
    grandkid = _plain(k, "og5_grandkid", parent=kid)
    r = await _as(k, owner, _upd(grandkid, description="mine"))
    assert r.get("updated") is True, r
    assert k.get(grandkid)["description"] == "mine"
    r = await _as(k, owner, _upd(owner, description="me"))
    assert r.get("updated") is True, r
    r = await _as(k, owner, _del(kid))
    assert r.get("deleted") is True, r
    assert k.ctx.agents.get(grandkid) is None


async def test_own_subtree_keeps_the_self_grant_rule(seeded_kernel):
    k = seeded_kernel
    owner = _plain(k, "og6_owner")
    r = await _as(k, owner, _upd(owner, can_call={**_BASE, k.id: "*"}))
    assert r.get("reason") == "unauthorized" and r.get("side") == "self-grant", r


async def test_own_subtree_keeps_the_holder_mark_rule(seeded_kernel):
    k = seeded_kernel
    owner = _plain(k, "og7_owner")
    vault = _plain(k, "og7_vault", parent=owner, holds_secrets=True)
    r = await _as(k, owner, _upd(vault, holds_secrets=None))
    assert r.get("reason") == "unauthorized" and r.get("side") == "secret-holder", r
    assert k.get(vault)["holds_secrets"] is True


async def test_own_subtree_keeps_the_delegation_rule(seeded_kernel):
    k = seeded_kernel
    other = _plain(k, "og8_other")
    owner = _plain(k, "og8_owner")
    kid = _plain(k, "og8_kid", parent=owner)
    r = await _as(k, owner, _upd(kid, can_call={**_BASE, other: ["reflect"]}))
    assert r.get("reason") == "unauthorized" and r.get("side") == "delegation", r


# ─── anywhere else: both halves, naming the operand ──────────────────────────


async def test_an_unrelated_operand_needs_egress_naming_it_and_its_ingress(
    seeded_kernel,
):
    k = seeded_kernel
    victim = _plain(k, "og9_victim", description="before")
    editor = _plain(
        k, "og9_editor", can_call={**_BASE, victim: ["update_agent", "delete_agent"]}
    )
    # egress names it; the operand has not accepted the editor yet
    r = await _as(k, editor, _upd(victim, description="x"))
    assert r.get("reason") == "unauthorized" and r.get("side") == "ingress", r
    assert r["error"] == f"{victim} does not accept 'update_agent' from {editor}", r
    r = await _as(k, editor, _del(victim))
    assert r.get("reason") == "unauthorized" and r.get("side") == "ingress", r
    # the operator opens the operand's ingress → both halves granted
    await _as(
        k,
        None,
        _upd(victim, callable_by={**_BASE, editor: ["update_agent", "delete_agent"]}),
    )
    r = await _as(k, editor, _upd(victim, description="after"))
    assert r.get("updated") is True, r
    r = await _as(k, editor, _del(victim))
    assert r.get("deleted") is True, r


async def test_the_root_record_is_an_operand_like_any_other(seeded_kernel):
    """`<root>: ["update_agent"]` now means what it says: edit the ROOT."""
    k = seeded_kernel
    editor = _plain(k, "og10_editor", can_call={**_BASE, k.id: ["update_agent"]})
    # the root's baseline callable_by accepts its direct children
    r = await _as(k, editor, _upd(k.id, description="root edited"))
    assert r.get("updated") is True, r


async def test_a_heritable_entry_naming_the_operand_reaches_it(seeded_kernel):
    k = seeded_kernel
    victim = _plain(k, "og11_victim", callable_by={**_BASE, "og11_kid": ["*"]})
    hub = _plain(
        k,
        "og11_hub",
        can_call={
            **_BASE,
            victim: {"verbs": ["update_agent"], "for": "descendants"},
        },
    )
    kid = _plain(k, "og11_kid", parent=hub)
    r = await _as(k, kid, _upd(victim, description="via hub"))
    assert r.get("updated") is True, r
    # the holder itself is never granted by its heritable entry
    r = await _as(k, hub, _upd(victim, description="x"))
    assert r.get("reason") == "unauthorized" and r.get("side") == "egress", r


async def test_the_operator_is_ungated(seeded_kernel):
    k = seeded_kernel
    victim = _plain(k, "og12_victim")
    r = await _as(k, None, _upd(victim, description="op"))
    assert r.get("updated") is True, r
    r = await _as(k, None, _del(victim))
    assert r.get("deleted") is True, r


async def test_an_unknown_operand_is_answered_by_the_verb(seeded_kernel):
    """No operand to gate: the root grant decides, and the verb says what is
    wrong (as an unknown `parent_id` does for create_agent)."""
    k = seeded_kernel
    builder = _plain(k, "og13_builder", can_call={**_BASE, k.id: ["update_agent"]})
    r = await _as(k, builder, _upd("og13_nobody", description="x"))
    assert r.get("reason") == "unknown_agent", r


# ─── a relayed foreign caller owns no subtree here (audit G-1) ───────────────


async def _relayed(kernel, leg, caller, payload):
    """`payload` to the root as bridge leg `leg` relaying the foreign `caller`."""
    item = {"target": "kernel", "payload": payload}
    with sender_context(leg), remote_caller_context(leg, caller):
        return (await kernel.send([item]))[0]["result"]


async def test_a_relayed_caller_does_not_inherit_the_legs_subtree(seeded_kernel):
    """Measured before the fix: a foreign caller a leg relays — refused `reflect`
    on the leg's child — edited and deleted that child and re-pinned the leg's
    own record: the gate took the LEG's own-subtree baseline on its behalf."""
    k = seeded_kernel
    leg = _plain(k, "og14_leg", peer_guid="g9")
    kid = _plain(k, "og14_kid", parent=leg, description="before")
    for payload in (
        _upd(kid, description="pwned"),
        _del(kid),
        _upd(leg, peer_guid="g0"),
    ):
        r = await _relayed(k, leg, "g9|mallory", payload)
        assert r.get("reason") == "unauthorized" and r.get("side") == "egress", r
        assert r["error"] == f"{leg} may not {payload['type']} {payload['id']}", r
    assert k.get(kid)["description"] == "before"
    assert k.get(leg)["peer_guid"] == "g9"
    # the leg on its OWN behalf keeps its baseline
    r = await _as(k, leg, _upd(kid, description="mine"))
    assert r.get("updated") is True, r


async def test_a_relayed_caller_is_judged_on_the_operand_as_its_qualified_id(
    seeded_kernel,
):
    """Egress is the leg's `can_call` naming the operand; ingress is the
    operand's `callable_by` against `<guid>|<agent>` — accepting the leg (the
    child's `$parent`) is not accepting its peer."""
    k = seeded_kernel
    leg = _plain(k, "og15_leg", can_call={**_BASE, "og15_kid": ["update_agent"]})
    kid = _plain(k, "og15_kid", parent=leg, description="before")
    r = await _relayed(k, leg, "g9|scout", _upd(kid, description="x"))
    assert r.get("reason") == "unauthorized" and r.get("side") == "ingress", r
    assert r["error"] == f"{kid} does not accept 'update_agent' from g9|scout", r
    await _as(k, None, _upd(kid, callable_by={**_BASE, "g9|scout": ["update_agent"]}))
    r = await _relayed(k, leg, "g9|scout", _upd(kid, description="after"))
    assert r.get("updated") is True, r
    assert k.get(kid)["description"] == "after"


async def test_a_relayed_caller_needs_the_roots_callable_by_on_kernel_verbs(
    seeded_kernel,
):
    """Measured before the fix (audit 2026-10-03, AV-3): kernel verbs at the root
    skip ingress, so a foreign caller relayed by a direct child of the root got
    `list_agents` on the leg's `$parent:*` alone while `reflect` on the same root
    was refused. The root's `callable_by` must accept `<guid>|<agent>` or
    `<guid>|*`; a local caller is unchanged."""
    k = seeded_kernel
    leg = _plain(k, "og16_leg", can_call={**_BASE, k.id: ["ps"]})
    for verb in ("list_agents", "ps"):
        r = await _relayed(k, leg, "g9|mallory", {"type": verb})
        assert r.get("reason") == "unauthorized" and r.get("side") == "ingress", r
        assert r["error"] == f"{k.id} does not accept {verb!r} from g9|mallory", r
        assert r["hint"] == (
            "a foreign caller is accepted only by name: callable_by "
            f"{{'g9|mallory': [...]}} or {{'g9|*': [...]}} on {k.id}"
        ), r
        # the leg on its OWN behalf: egress is the whole check, as before
        r = await _as(k, leg, {"type": verb})
        assert "error" not in r, r
    root_cb = dict(k._meta.get("callable_by") or {})
    await _as(
        k,
        None,
        _upd(
            k.id, callable_by={**root_cb, "g9|*": ["list_agents"], "g9|mallory": ["ps"]}
        ),
    )
    r = await _relayed(k, leg, "g9|mallory", {"type": "list_agents"})
    assert "error" not in r and "agents" in r, r
    assert "error" not in await _relayed(k, leg, "g9|mallory", {"type": "ps"})
    # a guid the root does not name is still refused
    r = await _relayed(k, leg, "g7|eve", {"type": "list_agents"})
    assert r.get("side") == "ingress", r
