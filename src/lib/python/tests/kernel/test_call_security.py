"""Call security wired into the kernel — the two-sided gate in `Agent.send`.

The model is **fully-gated, two-sided, from-above-only**:

- An AGENT-originated send (driven here via `sender_context(<id>)`) is checked
  TWICE: **egress** (the sender's `can_call` allows the target+verb) AND
  **ingress** (the target's `callable_by` accepts the sender+verb). The gate
  NEVER branches on a verb name — `reflect` included, nothing exempt.
- An OPERATOR send (no `_current_sender` — the CLI / bootstrap / loader-at-boot /
  a test driver calling `kernel.send` directly) is the ORIGIN root of trust and
  passes ungated.
- Authority flows from ABOVE only: an agent can never raise its own
  `can_call`/`callable_by`, and a parent may delegate only a SUBSET of its own
  reach.

Every agent carries the structural baseline `{$self:*, $parent:*, $children:*}`
on BOTH fields — reach/accept your own subtree and your creator. A LATERAL edge
between two siblings needs BOTH an explicit `can_call` on the caller and an
explicit `callable_by` on the callee.
"""

from __future__ import annotations


from kernel import sender_context


async def _mk(kernel, **meta) -> str:
    """Create a plain file_bridge child of root (opened so its OWN ingress gate
    never masks the call gate we're testing), returning its id. Created as
    operator → carries the stamped structural baseline on both fields."""
    payload = {
        "type": "create_agent",
        "handler_module": "file_bridge.tools",
        "ingress_rule": "allow_all_ingress",
    }
    payload.update(meta)
    rec = await kernel.send(kernel.id, payload)
    assert "id" in rec, rec
    return rec["id"]


# ─── operator vs agent boundary ─────────────────────────────────


async def test_operator_send_is_ungated(seeded_kernel):
    # No sender_context → operator → anything passes, including mutation.
    a = await _mk(seeded_kernel)
    r = await seeded_kernel.send(a, {"type": "list", "path": ""})
    assert "error" not in r or r.get("reason") != "unauthorized"


async def test_agent_send_to_ungranted_peer_is_denied(seeded_kernel):
    a = await _mk(seeded_kernel)
    b = await _mk(seeded_kernel)
    with sender_context(a):
        r = await seeded_kernel.send(b, {"type": "list", "path": ""})
    assert r.get("reason") == "unauthorized"
    assert r.get("side") == "egress"  # a has no can_call reach to b
    assert r.get("hint")  # the denial teaches how to open it — from above


# ─── the structural baseline (both fields) ──────────────────────


async def test_agent_may_call_self_and_parent_by_default(seeded_kernel):
    a = await _mk(seeded_kernel)
    with sender_context(a):
        # $self — a's can_call($self) + a's callable_by($self)
        r_self = await seeded_kernel.send(a, {"type": "list", "path": ""})
        # $parent is the root: a's can_call($parent) + root's callable_by($children)
        r_parent = await seeded_kernel.send(seeded_kernel.id, {"type": "list_agents"})
    assert r_self.get("reason") != "unauthorized"
    assert r_parent.get("reason") != "unauthorized"


async def test_reflect_is_gated_like_any_verb(seeded_kernel):
    # NOTHING is exempt — reflecting an ungranted lateral peer is denied.
    a = await _mk(seeded_kernel)
    b = await _mk(seeded_kernel)
    with sender_context(a):
        denied = await seeded_kernel.send(b, {"type": "reflect"})
    assert denied.get("reason") == "unauthorized"
    # Grant it from above on BOTH ends → reflect goes through.
    a2 = await _mk(seeded_kernel, can_call={b: ["reflect"]})
    await seeded_kernel.send(
        seeded_kernel.id,
        {"type": "update_agent", "id": b, "callable_by": {a2: ["reflect"]}},
    )
    with sender_context(a2):
        ok = await seeded_kernel.send(b, {"type": "reflect"})
    assert ok.get("reason") != "unauthorized" and ok.get("id") == b


# ─── two-sided: both ends must grant the lateral edge ───────────


async def test_lateral_edge_needs_both_egress_and_ingress(seeded_kernel):
    b = await _mk(seeded_kernel)
    # a is granted `list` on b (egress) — but b does not accept a (ingress).
    a = await _mk(seeded_kernel, can_call={b: ["list"]})
    with sender_context(a):
        r = await seeded_kernel.send(b, {"type": "list", "path": ""})
    assert r.get("reason") == "unauthorized"
    assert r.get("side") == "ingress"  # egress passed; the callee refused

    # Open the ingress from above → the edge exists.
    await seeded_kernel.send(
        seeded_kernel.id,
        {"type": "update_agent", "id": b, "callable_by": {a: ["list"]}},
    )
    with sender_context(a):
        ok = await seeded_kernel.send(b, {"type": "list", "path": ""})
        no = await seeded_kernel.send(b, {"type": "mkdir", "path": "x"})
    assert ok.get("reason") != "unauthorized"
    assert no.get("reason") == "unauthorized"  # only `list` was granted, both ends


# ─── privileged verbs ───────────────────────────────────────────


async def test_agent_cannot_create_agent_even_on_its_parent(seeded_kernel):
    # A depth-1 agent's $parent IS the root, and its default grant is "*" — but
    # create_agent is PRIVILEGED, so the wildcard does not cover it.
    a = await _mk(seeded_kernel)
    with sender_context(a):
        r = await seeded_kernel.send(
            seeded_kernel.id,
            {"type": "create_agent", "handler_module": "file_bridge.tools"},
        )
    assert r.get("reason") == "unauthorized"


async def test_explicit_kernel_grant_lets_an_agent_create(seeded_kernel):
    # Grant a the privileged verb explicitly → it can mutate through the root.
    # (Ingress: the root accepts a via its $children baseline.)
    a = await _mk(seeded_kernel, can_call={seeded_kernel.id: ["create_agent"]})
    with sender_context(a):
        r = await seeded_kernel.send(
            seeded_kernel.id,
            {"type": "create_agent", "handler_module": "file_bridge.tools"},
        )
    assert r.get("reason") != "unauthorized" and "id" in r


# ─── authority flows from ABOVE only ────────────────────────────


async def test_agent_cannot_raise_its_own_authority(seeded_kernel):
    # update_agent on self, editing own can_call → self-grant, denied.
    a = await _mk(seeded_kernel, can_call={seeded_kernel.id: ["update_agent"]})
    b = await _mk(seeded_kernel)
    with sender_context(a):
        r = await seeded_kernel.send(
            seeded_kernel.id,
            {"type": "update_agent", "id": a, "can_call": {b: ["list"]}},
        )
    assert r.get("reason") == "unauthorized"
    assert r.get("side") == "self-grant"


async def test_parent_confers_only_a_subset_it_holds(seeded_kernel):
    peer = await _mk(seeded_kernel)
    # granter can reach `peer` for `list`, and may create/update through root.
    granter = await _mk(
        seeded_kernel,
        can_call={peer: ["list"], seeded_kernel.id: ["create_agent", "update_agent"]},
    )
    with sender_context(granter):
        # Confer a SUBSET it holds (peer:list) onto a new child → allowed.
        ok = await seeded_kernel.send(
            seeded_kernel.id,
            {
                "type": "create_agent",
                "handler_module": "file_bridge.tools",
                "can_call": {peer: ["list"]},
            },
        )
        # Confer MORE than it holds (peer:mkdir) → refused as over-delegation.
        no = await seeded_kernel.send(
            seeded_kernel.id,
            {
                "type": "create_agent",
                "handler_module": "file_bridge.tools",
                "can_call": {peer: ["mkdir"]},
            },
        )
    assert ok.get("reason") != "unauthorized" and "id" in ok
    assert no.get("reason") == "unauthorized"
    assert no.get("side") == "delegation"


async def test_callable_by_is_freely_conferrable(seeded_kernel):
    # Ingress grants no reach on their own, so a granter may confer any
    # `callable_by` (even one it could not itself satisfy).
    granter = await _mk(seeded_kernel, can_call={seeded_kernel.id: ["create_agent"]})
    with sender_context(granter):
        ok = await seeded_kernel.send(
            seeded_kernel.id,
            {
                "type": "create_agent",
                "handler_module": "file_bridge.tools",
                "callable_by": {"anyone_at_all": ["list"]},
            },
        )
    assert ok.get("reason") != "unauthorized" and "id" in ok


async def test_operator_conferral_is_unbounded(seeded_kernel):
    # The operator (no sender_context) may confer any can_call — from above.
    peer = await _mk(seeded_kernel)
    rec = await seeded_kernel.send(
        seeded_kernel.id,
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "can_call": {peer: ["list", "mkdir", "delete"]},
        },
    )
    assert "id" in rec
