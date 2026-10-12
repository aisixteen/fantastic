"""Call security wired into the kernel — the two-sided gate in `Agent.send`.

The model is **fully-gated, two-sided** — grants are the only rule:

- An AGENT-originated send (driven here via `as_agent(kernel, <id>)` — the test standing in for that agent's handler) is checked
  TWICE: **egress** (the sender's `can_call` allows the target+verb) AND
  **ingress** (the target's `callable_by` accepts the sender+verb). The gate
  NEVER branches on a verb name — `reflect` included, nothing exempt.
- An OPERATOR send (no `_current_sender` — the CLI / bootstrap / loader-at-boot /
  a test driver calling `kernel.send` directly) is the ORIGIN root of trust and
  passes ungated.
- The tree verbs take the gate like any verb and nothing more (owner,
  2026-10-06): an agent granted `update_agent` on the root may edit ANY record,
  its own grants included — holding that grant IS the authority.

Every agent carries the baseline `{$self:*}` on BOTH fields — nothing else. Any
other edge (parent, child, sibling, the root) needs BOTH an explicit `can_call`
on the caller and an explicit `callable_by` on the callee.
"""

from __future__ import annotations


from _testkit import as_agent

from _testkit import wire


async def _mk(kernel, **meta) -> str:
    """Create a plain file_bridge child of root (opened so its OWN ingress gate
    never masks the call gate we're testing), returning its id. Created as
    operator → carries the stamped baseline on both fields."""
    payload = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "handler_module": "file_bridge.tools",
        "allow": ["."],
        "ingress_rule": "allow_all_ingress",
    }
    payload.update(meta)
    rec = (await kernel.send([{"target": kernel.id, "payload": payload}]))[0]["result"]
    assert "id" in rec, rec
    return rec["id"]


# ─── operator vs agent boundary ─────────────────────────────────


async def test_operator_send_is_ungated(seeded_kernel):
    # No as_agent → operator → anything passes, including mutation.
    a = await _mk(seeded_kernel)
    r = (
        await seeded_kernel.send(
            [{"target": a, "payload": {"type": "list", "path": ""}}]
        )
    )[0]["result"]
    assert "error" not in r or r.get("reason") != "unauthorized"


async def test_agent_send_to_ungranted_peer_is_denied(seeded_kernel):
    a = await _mk(seeded_kernel)
    b = await _mk(seeded_kernel)
    with as_agent(seeded_kernel, a):
        r = (
            await seeded_kernel.send(
                [{"target": b, "payload": {"type": "list", "path": ""}}]
            )
        )[0]["result"]
    assert r.get("reason") == "unauthorized"
    assert r.get("side") == "egress"  # a has no can_call reach to b
    assert r.get("hint")  # the denial teaches how to open it — from above


# ─── the baseline (both fields): `$self` only ───────────────────


async def test_agent_may_call_only_itself_by_default(seeded_kernel):
    a = await _mk(seeded_kernel)
    with as_agent(seeded_kernel, a):
        # $self — a's can_call($self) + a's callable_by($self)
        r_self = (
            await seeded_kernel.send(
                [{"target": a, "payload": {"type": "list", "path": ""}}]
            )
        )[0]["result"]
        # its parent (the root) is NOT in the baseline
        r_parent = (
            await seeded_kernel.send(
                [{"target": seeded_kernel.id, "payload": {"type": "list_agents"}}]
            )
        )[0]["result"]
    assert r_self.get("reason") != "unauthorized"
    assert r_parent.get("reason") == "unauthorized" and r_parent["side"] == "egress"


async def test_reflect_is_gated_like_any_verb(seeded_kernel):
    # NOTHING is exempt — reflecting an ungranted lateral peer is denied.
    a = await _mk(seeded_kernel)
    b = await _mk(seeded_kernel)
    with as_agent(seeded_kernel, a):
        denied = (
            await seeded_kernel.send([{"target": b, "payload": {"type": "reflect"}}])
        )[0]["result"]
    assert denied.get("reason") == "unauthorized"
    # Grant it from above on BOTH ends → reflect goes through.
    a2 = await _mk(seeded_kernel, can_call={b: ["reflect"]})
    (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "update_agent",
                        "id": b,
                        "callable_by": {a2: ["reflect"]},
                    },
                }
            ]
        )
    )[0]["result"]
    with as_agent(seeded_kernel, a2):
        ok = (
            await seeded_kernel.send([{"target": b, "payload": {"type": "reflect"}}])
        )[0]["result"]
    assert ok.get("reason") != "unauthorized" and ok.get("id") == b


# ─── two-sided: both ends must grant the lateral edge ───────────


async def test_lateral_edge_needs_both_egress_and_ingress(seeded_kernel):
    b = await _mk(seeded_kernel)
    # a is granted `list` on b (egress) — but b does not accept a (ingress).
    a = await _mk(seeded_kernel, can_call={b: ["list"]})
    with as_agent(seeded_kernel, a):
        r = (
            await seeded_kernel.send(
                [{"target": b, "payload": {"type": "list", "path": ""}}]
            )
        )[0]["result"]
    assert r.get("reason") == "unauthorized"
    assert r.get("side") == "ingress"  # egress passed; the callee refused

    # Open the ingress from above → the edge exists.
    (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "update_agent",
                        "id": b,
                        "callable_by": {a: ["list"]},
                    },
                }
            ]
        )
    )[0]["result"]
    with as_agent(seeded_kernel, a):
        ok = (
            await seeded_kernel.send(
                [{"target": b, "payload": {"type": "list", "path": ""}}]
            )
        )[0]["result"]
        no = (
            await seeded_kernel.send(
                [{"target": b, "payload": {"type": "mkdir", "path": "x"}}]
            )
        )[0]["result"]
    assert ok.get("reason") != "unauthorized"
    assert no.get("reason") == "unauthorized"  # only `list` was granted, both ends


# ─── mutators ───────────────────────────────────────────────────


async def test_agent_cannot_create_agent_even_on_its_parent(seeded_kernel):
    # The baseline reaches nothing but the agent itself.
    a = await _mk(seeded_kernel)
    with as_agent(seeded_kernel, a):
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": seeded_kernel.id,
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                        },
                    }
                ]
            )
        )[0]["result"]
    assert r.get("reason") == "unauthorized"


async def test_explicit_kernel_grant_lets_an_agent_create(seeded_kernel):
    # Grant `create_agent` on BOTH ends of a → root → it creates its own child.
    a = await _mk(seeded_kernel, can_call={seeded_kernel.id: ["create_agent"]})
    wire(seeded_kernel, a, seeded_kernel.id, ["create_agent"])
    with as_agent(seeded_kernel, a):
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": a,
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                        },
                    }
                ]
            )
        )[0]["result"]
    assert r.get("reason") != "unauthorized" and "id" in r


# ─── authority is the grant ─────────────────────────────────────


async def test_update_agent_on_the_root_is_the_authority_over_records(seeded_kernel):
    # Granted update_agent on the root (both ends), an agent edits its OWN can_call:
    # the grant is the authority — no ownership rule after the gate.
    a = await _mk(seeded_kernel)
    wire(seeded_kernel, a, seeded_kernel.id, ["update_agent"])
    b = await _mk(seeded_kernel)
    with as_agent(seeded_kernel, a):
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": seeded_kernel.id,
                        "payload": {
                            "type": "update_agent",
                            "id": a,
                            "can_call": {b: ["list"]},
                        },
                    }
                ]
            )
        )[0]["result"]
    assert r.get("updated") is True, r
    assert seeded_kernel.ctx.agents[a]._meta["can_call"].get(b) == ["list"]


async def test_without_the_grant_an_agent_cannot_raise_its_own_authority(seeded_kernel):
    a = await _mk(seeded_kernel)
    b = await _mk(seeded_kernel)
    with as_agent(seeded_kernel, a):
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": seeded_kernel.id,
                        "payload": {
                            "type": "update_agent",
                            "id": a,
                            "can_call": {b: ["list"]},
                        },
                    }
                ]
            )
        )[0]["result"]
    assert r.get("reason") == "unauthorized" and r.get("side") == "egress", r


async def test_a_parent_writes_any_grant_on_its_own_child(seeded_kernel):
    # Grants come from above: a parent writes any grant on a child it creates.
    granter = await _mk(seeded_kernel)
    wire(seeded_kernel, granter, seeded_kernel.id, ["create_agent"])
    with as_agent(seeded_kernel, granter):
        ok = (
            await seeded_kernel.send(
                [
                    {
                        "target": seeded_kernel.id,
                        "payload": {
                            "type": "create_agent",
                            "parent_id": granter,
                            "handler_module": "file_bridge.tools",
                            "allow": ["."],
                            "callable_by": {"anyone_at_all": ["list"]},
                            "can_call": {"anything": "*"},
                        },
                    }
                ]
            )
        )[0]["result"]
    assert ok.get("reason") != "unauthorized" and "id" in ok


async def test_operator_conferral_is_unbounded(seeded_kernel):
    # The operator (no as_agent) may write any can_call — from above.
    peer = await _mk(seeded_kernel)
    rec = (
        await seeded_kernel.send(
            [
                {
                    "target": seeded_kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": seeded_kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "can_call": {peer: ["list", "mkdir", "delete"]},
                    },
                }
            ]
        )
    )[0]["result"]
    assert "id" in rec


# ─── grant SHAPE is refused at WRITE time ───────────────────────


async def test_update_agent_refuses_a_non_dict_grant(kernel):
    """`can_call:"*"` (a bare string) used to be accepted — `{"updated": true}` —
    and the gate then raised `.items()` out of a later dispatch (measured
    2026-09-01, E21_plan: a correctly-opened web leg answered 500). The shape is
    the WRITER's to refuse, at the point of the mistake."""
    aid = await _mk(kernel)
    r = (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {"type": "update_agent", "id": aid, "can_call": "*"},
                }
            ]
        )
    )[0]["result"]
    assert r.get("error") and "grant MAP" in r["error"], r
    # the record is untouched — still a dict (the stamped baseline)
    assert isinstance(kernel.ctx.agents[aid].record.get("can_call"), dict)


async def test_create_agent_refuses_a_non_dict_grant(kernel):
    r = (
        await kernel.send(
            [
                {
                    "target": kernel.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "callable_by": ["*"],
                    },
                }
            ]
        )
    )[0]["result"]
    assert r.get("error") and "grant MAP" in r["error"], r
