"""The distilled roster is GATED per node.

`reflect` is not exempt from the call gate anywhere else, but `_roster` walked
the tree dispatching `reflect` directly, so a caller allowed to reflect ONE agent
read every descendant's `sentence` and `description` out of the tree it got back
— exactly the lateral discovery the two-sided gate exists to close (measured
2026-09-02). The SHAPE stays public (`tree=ids` already exposes ids); what an
agent SAYS about itself does not.
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


def _find(node, agent_id):
    if node.get("id") == agent_id:
        return node
    for kid in node.get("children") or []:
        hit = _find(kid, agent_id)
        if hit is not None:
            return hit
    return None


async def test_the_roster_omits_what_the_caller_may_not_reflect(seeded_kernel):
    visible = await _mk(
        seeded_kernel,
        "rg_visible",
        description="the visible one",
        # Two-sided: the reader's egress AND this agent's ingress.
        callable_by={
            "$self": "*",
            "$parent": "*",
            "$children": "*",
            "rg_reader": ["reflect"],
        },
    )
    secret = await _mk(
        seeded_kernel, "rg_secret", parent=visible, description="the secret one"
    )
    reader = await _mk(
        seeded_kernel,
        "rg_reader",
        can_call={
            "$self": "*",
            "$parent": "*",
            "$children": "*",
            seeded_kernel.id: ["reflect"],
            visible: ["reflect"],
        },
    )
    with sender_context(reader):
        r = (
            await seeded_kernel.send(
                [{"target": "kernel", "payload": {"type": "reflect"}}]
            )
        )[0]["result"]
    tree = r["tree"]
    seen = _find(tree, visible)
    assert seen is not None and seen.get("description") == "the visible one", seen
    hidden = _find(tree, secret)
    assert hidden is not None, "the SHAPE stays public — the id is still a node"
    assert "description" not in hidden, hidden
    assert "sentence" not in hidden, hidden

    # The operator sees both, as everywhere else.
    op = (
        await seeded_kernel.send([{"target": "kernel", "payload": {"type": "reflect"}}])
    )[0]["result"]
    op_secret = _find(op["tree"], secret)
    assert op_secret.get("description") == "the secret one", op_secret


async def test_the_full_tree_applies_the_same_per_node_gate(seeded_kernel):
    """Measured before the fix (audit 2026-10-03, KV-3): `detail:"full"` walked
    the wiring tree with no gate, so a depth-2 agent refused `reflect` on its
    sibling read that sibling's `description` and grant maps by reflecting its
    own parent (baseline `$parent:*`). A denied node keeps `id` + `children`."""
    parent = await _mk(seeded_kernel, "rgf_parent", description="the parent")
    sibling = await _mk(
        seeded_kernel,
        "rgf_sibling",
        parent=parent,
        description="the prod deploy key",
        can_call={"$self": "*", "$parent": "*", "$children": "*", "rgf_peer": "*"},
    )
    kid = await _mk(seeded_kernel, "rgf_kid", parent=sibling, description="deeper")
    reader = await _mk(seeded_kernel, "rgf_reader", parent=parent)
    with sender_context(reader):
        direct = (
            await seeded_kernel.send(
                [{"target": sibling, "payload": {"type": "reflect"}}]
            )
        )[0]["result"]
        r = (
            await seeded_kernel.send(
                [{"target": parent, "payload": {"type": "reflect", "detail": "full"}}]
            )
        )[0]["result"]
    assert direct.get("reason") == "unauthorized", direct
    tree = r["tree"]
    assert tree["id"] == parent and tree.get("description") == "the parent", tree
    for hidden_id in (sibling, kid):
        hidden = _find(tree, hidden_id)
        assert hidden is not None, "the SHAPE stays public — the id is still a node"
        assert set(hidden) == {"id", "children"}, hidden
    assert [c["id"] for c in _find(tree, sibling)["children"]] == [kid]
    # the reader's own node is reflectable to itself
    assert _find(tree, reader).get("handler_module") == "file_bridge.tools"

    # The operator sees everything, as everywhere else.
    op = (
        await seeded_kernel.send(
            [{"target": parent, "payload": {"type": "reflect", "detail": "full"}}]
        )
    )[0]["result"]
    op_sib = _find(op["tree"], sibling)
    assert op_sib.get("description") == "the prod deploy key", op_sib
    assert "rgf_peer" in op_sib.get("can_call", {}), op_sib
