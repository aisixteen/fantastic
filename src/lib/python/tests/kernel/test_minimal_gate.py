"""The MINIMAL call gate.

- An agent send passes iff the sender's `can_call` grants (target, verb) AND the
  target's `callable_by` grants (sender, verb); `"*"` covers EVERY verb.
- The baseline on both fields is `{"$self": "*"}` — nothing else.
- Kernel verbs at the root take the same two-sided check against the root.
- The tree verbs (`create_agent`, `update_agent`, `delete_agent`) take the gate
  like any verb and nothing more: whoever holds them on the root acts on any agent
  (grants are the only rule).
- A grant key is an agent id, `$self`, `$parent`, `$subtree`, `<guid>|<agent>`
  or `<guid>|*` — anything else is refused at the write.
"""

from __future__ import annotations

import types

import pytest

from kernel import register_unlisted

from _testkit import as_agent
from kernel._authz import DEFAULT_CALLABLE_BY, DEFAULT_CAN_CALL, authorize_call

_mod = types.ModuleType("minimal_gate_probe.tools")


async def _handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    return {"ok": id, "verb": verb}


_mod.handler = _handler
_mod.VERBS = {}
register_unlisted("minimal_gate_probe.tools", _mod)
MOD = "minimal_gate_probe.tools"


def _mk(k, aid: str, parent=None, **meta) -> str:
    owner = k if parent is None else k.ctx.agents[parent]
    rec = owner.create(MOD, id=aid, **meta)
    assert rec.get("id") == aid, rec
    return aid


async def _as(k, sender: str, target: str, payload: dict) -> dict:
    with as_agent(k, sender):
        return (await k.send([{"target": target, "payload": payload}]))[0]


def _open_root(k, aid: str, verbs="*") -> None:
    """`aid` → the root on both ends."""
    k.ctx.update(aid, can_call={"$self": "*", "kernel_state": verbs})
    k.ctx.update("kernel_state", callable_by={"$self": "*", aid: verbs})
# twin: the_baseline_is_self_only_on_both_fields
# twin: baseline_is_self_only


def test_the_baseline_is_self_only():
    assert DEFAULT_CAN_CALL == {"$self": "*"}
    assert DEFAULT_CALLABLE_BY == {"$self": "*"}


async def test_self_token_resolves_to_holder_id(seeded_kernel):
    k = seeded_kernel
    _mk(k, "boss")
    _mk(k, "kid", parent="boss")
    assert k.ctx.agents["kid"]._meta["can_call"] == {"$self": "*"}
    up = (await _as(k, "kid", "boss", {"type": "ping"}))["result"]
    assert up["reason"] == "unauthorized" and up["side"] == "egress", up
    down = (await _as(k, "boss", "kid", {"type": "ping"}))["result"]
    assert down["reason"] == "unauthorized" and down["side"] == "egress", down
    self_call = await _as(k, "kid", "kid", {"type": "ping"})
    assert self_call["status"] == "completed", self_call
# twin: both_ends_must_grant_the_edge
# twin: per_verb_list_grants_exactly_those


async def test_two_sided_allow_and_deny(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a", can_call={"b": ["ping"]})
    _mk(k, "b")
    r = (await _as(k, "a", "b", {"type": "ping"}))["result"]
    assert r["side"] == "ingress", r
    k.ctx.update("b", callable_by={"$self": "*", "a": ["ping"]})
    ok = await _as(k, "a", "b", {"type": "ping"})
    assert ok["status"] == "completed" and ok["result"] == {"ok": "b", "verb": "ping"}
    other = (await _as(k, "a", "b", {"type": "pong"}))["result"]
    assert other["side"] == "egress", other


async def test_subtree_token_matches_every_depth_below(seeded_kernel):
    """`$subtree` matches every agent BELOW the holder — a grandchild included,
    on both sides of the gate; an agent outside the subtree stays refused."""
    k = seeded_kernel
    _mk(k, "top", can_call={"$subtree": "*"}, callable_by={"$subtree": ["up"]})
    _mk(k, "mid", parent="top")
    _mk(k, "deep", parent="mid", can_call={"top": "*"}, callable_by={"top": "*"})
    _mk(k, "outside", can_call={"top": "*"})
    assert (await _as(k, "top", "deep", {"type": "down"}))["status"] == "completed"
    assert (await _as(k, "deep", "top", {"type": "up"}))["status"] == "completed"
    r = (await _as(k, "outside", "top", {"type": "up"}))["result"]
    assert r["side"] == "ingress", r


async def test_parent_token_resolves_to_parent_id(seeded_kernel):
    k = seeded_kernel
    _mk(k, "boss", can_call={"$subtree": "*"}, callable_by={"$subtree": ["up"]})
    _mk(
        k, "kid", parent="boss", can_call={"$parent": "*"}, callable_by={"$parent": "*"}
    )
    assert (await _as(k, "boss", "kid", {"type": "down"}))["status"] == "completed"
    assert (await _as(k, "kid", "boss", {"type": "up"}))["status"] == "completed"
    r = (await _as(k, "kid", "boss", {"type": "sideways"}))["result"]
    assert r["side"] == "ingress", r
# twin: star_covers_every_verb_including_the_mutators
# twin: a_peer_target_matches_only_by_name


def test_star_covers_every_verb_and_tokens_never_match_a_foreign_id():
    g = {"x": "*"}
    for verb in (
        "create_agent",
        "update_agent",
        "delete_agent",
        "ps",
        "cancel",
        "watch",
        "state_subscribe",
        "stop",
        "boot",
        "ledger",
    ):
        assert authorize_call(g, verb, "x", holder_id="h").allowed, verb
    parent = {"$parent": "*"}
    assert not authorize_call(
        parent, "ping", "g1|p", holder_id="h", holder_parent="g1|p"
    ).allowed
    assert authorize_call(
        {"g1|*": ["ping"]}, "ping", "g1|anyone", holder_id="h"
    ).allowed
    assert not authorize_call(
        {"g1|*": ["ping"]}, "ping", "g2|anyone", holder_id="h"
    ).allowed


async def test_star_covers_create_agent(seeded_kernel):
    k = seeded_kernel
    _mk(k, "maker")
    _open_root(k, "maker")
    out = await _as(
        k,
        "maker",
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "maker",
            "handler_module": MOD,
            "id": "made",
        },
    )
    assert out["status"] == "completed" and out["result"]["id"] == "made", out
    assert k.ctx.agents["made"].parent.id == "maker"


async def test_kernel_verbs_at_the_root_check_the_roots_callable_by(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a", can_call={"kernel_state": ["ps"]})
    r = (await _as(k, "a", "kernel", {"type": "ps"}))["result"]
    assert r == {
        "error": "kernel_state does not accept 'ps' from a",
        "reason": "unauthorized",
        "hint": (
            "grants come from above — an ancestor (update_agent on an agent in its "
            "subtree) or the operator writes can_call and callable_by; an agent never "
            "raises its own"
        ),
        "side": "ingress",
    }
    k.ctx.update("kernel_state", callable_by={"$self": "*", "a": ["ps"]})
    ok = await _as(k, "a", "kernel", {"type": "ps"})
    assert ok["status"] == "completed" and "tasks" in ok["result"], ok


async def test_a_tree_verb_granted_on_the_root_acts_anywhere(seeded_kernel):
    """GRANTS ARE THE ONLY RULE: an agent granted the tree verbs
    on the root creates, updates and deletes ANY agent it names — outside its own
    subtree and itself included. There is no ownership check after the gate."""
    k = seeded_kernel
    _mk(k, "a")
    _mk(k, "peer")
    _open_root(k, "a")
    made = await _as(
        k,
        "a",
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "kernel_state",
            "handler_module": MOD,
            "id": "top",
        },
    )
    assert made["status"] == "completed" and "top" in k.ctx.agents, made
    up = await _as(
        k, "a", "kernel", {"type": "update_agent", "id": "peer", "description": "x"}
    )
    assert up["status"] == "completed" and up["result"].get("updated") is True, up
    me = await _as(
        k, "a", "kernel", {"type": "update_agent", "id": "a", "description": "y"}
    )
    assert me["status"] == "completed" and me["result"].get("updated") is True, me
    gone = await _as(k, "a", "kernel", {"type": "delete_agent", "id": "peer"})
    assert gone["status"] == "completed" and "peer" not in k.ctx.agents, gone
    nobody = (await _as(k, "a", "kernel", {"type": "delete_agent", "id": "nobody"}))[
        "result"
    ]
    assert nobody.get("side") is None and nobody.get("reason") != "unauthorized", nobody


@pytest.mark.parametrize(
    "payload, arg",
    [
        ({"type": "create_agent", "handler_module": MOD}, "parent_id"),
        ({"type": "create_agent", "handler_module": MOD, "parent_id": 7}, "parent_id"),
        ({"type": "update_agent", "description": "x"}, "id"),
        ({"type": "update_agent", "id": ["kid"], "description": "x"}, "id"),
        ({"type": "delete_agent"}, "id"),
        ({"type": "delete_agent", "id": ""}, "id"),
    ],
)
# twin: delete_agent_without_an_id_is_missing_arg
# twin: a_missing_operand_or_an_unknown_id_gets_the_verbs_own_refusal
async def test_a_missing_operand_gets_the_verbs_own_refusal(
    seeded_kernel, payload, arg
):
    """A tree verb whose operand is missing or not a string is the VERB's to refuse
    (`missing_arg`)."""
    k = seeded_kernel
    _mk(k, "a")
    _open_root(k, "a")
    r = (await _as(k, "a", "kernel", payload))["result"]
    assert (r["reason"], r["arg"]) == ("missing_arg", arg), r


async def test_a_mutator_on_a_direct_child_passes(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a")
    _mk(k, "kid", parent="a")
    _open_root(k, "a")
    up = await _as(
        k,
        "a",
        "kernel",
        {"type": "update_agent", "id": "kid", "can_call": {"$self": "*", "a": "*"}},
    )
    assert up["status"] == "completed" and up["result"].get("updated") is True, up
    assert k.ctx.agents["kid"]._meta["can_call"] == {"$self": "*", "a": "*"}
    gone = await _as(k, "a", "kernel", {"type": "delete_agent", "id": "kid"})
    assert gone["status"] == "completed" and "kid" not in k.ctx.agents, gone


async def test_an_ancestor_mutates_at_any_depth(seeded_kernel):
    """The sender's whole subtree is its own: a grandchild can be created under,
    updated and deleted by any agent above it."""
    k = seeded_kernel
    _mk(k, "a")
    _mk(k, "kid", parent="a")
    _mk(k, "grandkid", parent="kid")
    _open_root(k, "a")
    made = await _as(
        k,
        "a",
        "kernel",
        {
            "type": "create_agent",
            "parent_id": "kid",
            "handler_module": MOD,
            "id": "gk2",
        },
    )
    assert made["status"] == "completed" and "gk2" in k.ctx.agents, made
    up = await _as(
        k,
        "a",
        "kernel",
        {"type": "update_agent", "id": "grandkid", "description": "x"},
    )
    assert up["status"] == "completed" and up["result"].get("updated") is True, up
    gone = await _as(k, "a", "kernel", {"type": "delete_agent", "id": "grandkid"})
    assert gone["status"] == "completed" and "grandkid" not in k.ctx.agents, gone


async def test_a_mutator_is_gated_on_the_root_first(seeded_kernel):
    k = seeded_kernel
    _mk(k, "a")
    _mk(k, "kid", parent="a")
    r = (await _as(k, "a", "kernel", {"type": "delete_agent", "id": "kid"}))["result"]
    assert r["side"] == "egress" and "kid" in k.ctx.agents, r


@pytest.mark.parametrize(
    "key",
    ["$loader", "$subtree:a", "$root", "*", "", "|x", "g1|", "g1|a|b"],
)
# twin: unknown_grant_keys_and_bad_values_are_refused_at_write_time
# twin: removed_tokens_match_nothing
# twin: grant_keys_are_exactly_the_legal_set
async def test_an_unknown_grant_key_is_refused_at_the_write(seeded_kernel, key):
    k = seeded_kernel
    for field in ("can_call", "callable_by"):
        out = await k.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": MOD,
                        field: {key: "*"},
                    },
                }
            ]
        )
        r = out[0]["result"]
        assert r["reason"] == "invalid_meta", r
        legal = "an agent id, $self, $parent, $subtree" + (
            ", <guid>|<agent> or <guid>|*" if field == "can_call" else ""
        )
        assert r["error"] == (
            f"{field}[{key!r}] is not a grant key — legal keys: {legal}"
        ), r


async def test_a_bad_grant_value_is_refused_at_the_write(seeded_kernel):
    k = seeded_kernel
    for value in ({"verbs": ["x"], "for": "descendants"}, "all", 3, [1]):
        out = await k.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": MOD,
                        "can_call": {"x": value},
                    },
                }
            ]
        )
        r = out[0]["result"]
        assert r["reason"] == "invalid_meta", r
        assert r["error"] == "can_call['x'] must be \"*\" or a list of verb strings", r


async def test_legal_keys_are_accepted(seeded_kernel):
    k = seeded_kernel
    local = {"$self": "*", "$parent": ["a"], "$subtree": "*", "peer": ["b"]}
    out = await k.send(
        [
            {
                "target": "kernel",
                "payload": {
                    "type": "create_agent",
                    "parent_id": "kernel_state",
                    "handler_module": MOD,
                    "id": "legal",
                    # a peer kernel's agent is an OUTBOUND address: can_call only
                    "can_call": {**local, "g1|ai": "*", "g1|*": ["c"]},
                    "callable_by": local,
                },
            }
        ]
    )
    assert out[0]["result"].get("id") == "legal", out


async def test_a_deleted_id_leaves_every_grant(kernel):
    """Grant keys are literal ids: a deleted agent's id is pruned from every
    record that named it, so re-creating the id inherits nothing."""
    _mk(kernel, "vault", callable_by={"$self": "*", "reader": ["get"]})
    _mk(kernel, "reader", can_call={"$self": "*", "vault": ["get"]})
    _mk(kernel, "reader_kid", parent="reader")
    _mk(kernel, "other", callable_by={"$self": "*", "reader_kid": "*", "vault": "*"})
    out = await kernel.delete("reader")
    assert out["deleted"] is True
    assert kernel.ctx.agents["vault"]._meta["callable_by"] == {"$self": "*"}
    assert kernel.ctx.agents["other"]._meta["callable_by"] == {
        "$self": "*",
        "vault": "*",
    }
    _mk(kernel, "reader", can_call={"$self": "*", "vault": ["get"]})
    denied = await _as(kernel, "reader", "vault", {"type": "get"})
    assert denied["status"] == "failed" and denied["result"]["side"] == "ingress"


def test_empty_grant_denies():
    """Twin of the Rust test of the same name: an EXPLICIT empty grant `{}` denies,
    not only the default `{"$self": "*"}` (which reaches no peer either)."""
    assert not authorize_call({}, "send", "peer", holder_id="worker").allowed
    assert not authorize_call(
        {}, "send", "peer", holder_id="worker", holder_parent="boss"
    ).allowed
    # and the default grant still reaches nothing but the holder itself
    assert not authorize_call(
        {"$self": "*"}, "send", "peer", holder_id="worker"
    ).allowed


# ─── Rust twins (#170): each `test_<rust test name>` asserts what the Rust test does ─

_hop_log: list[str] = []


async def _hop_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb != "hop":
        return {"ok": id}
    _hop_log.append(id)
    rest = list(payload.get("next") or [])
    if not rest:
        return {"at": id}
    nxt = rest.pop(0)
    o = (
        await kernel.send([{"target": nxt, "payload": {"type": "hop", "next": rest}}])
    )[0]
    return {"via": id, "status": o["status"], "got": o["result"]}


_hop_mod = types.ModuleType("hop_twin_probe.tools")
_hop_mod.handler = _hop_handler
_hop_mod.VERBS = {}
register_unlisted("hop_twin_probe.tools", _hop_mod)


def _hop_chain(k) -> None:
    _hop_log.clear()
    for aid, can_call, callable_by in (
        ("h_a", {"h_b": "*"}, {}),
        ("h_b", {"h_c": "*"}, {"h_a": "*"}),
        ("h_c", {}, {"h_b": "*"}),
        ("h_d", {}, {}),
    ):
        rec = k.create(
            "hop_twin_probe.tools", id=aid, can_call=can_call, callable_by=callable_by
        )
        assert rec.get("id") == aid, rec


async def test_emit_on_another_agent_needs_both_halves(seeded_kernel):
    """An emit onto ANOTHER agent's inbox passes the two-sided gate under verb
    `emit`: no egress grant -> dropped, both halves granted -> delivered. Emitting on
    your own inbox is how every bundle publishes, so it stays ungated."""
    k = seeded_kernel
    _mk(k, "em_open", can_call={"em_target": ["emit"]})
    _mk(k, "em_closed", can_call={})
    _mk(k, "em_target", callable_by={"em_open": ["emit"], "em_closed": ["emit"]})
    rx = k.ctx.ensure_inbox("em_target")

    with as_agent(k, "em_closed"):
        await k.emit("em_target", {"type": "poke", "n": 1})
    assert rx.empty(), "an emit with no egress grant must be dropped"

    with as_agent(k, "em_open"):
        await k.emit("em_target", {"type": "poke", "n": 2})
    assert rx.get_nowait() == {"type": "poke", "n": 2}, "the granted emit landed"

    # emitting on your OWN inbox -- ungated
    with as_agent(k, "em_target"):
        await k.emit("em_target", {"type": "self", "n": 3})
    assert rx.get_nowait()["n"] == 3


async def test_a_handler_cannot_send_as_anyone_but_itself(seeded_kernel):
    """IDENTITY COMES FROM DISPATCH: h_a holds NO grant to h_d and h_d accepts nobody
    -- a nested send from h_a's handler is h_a's, and refused. There is no switch to
    make it anyone else's."""
    k = seeded_kernel
    _hop_chain(k)
    out = (
        await k.send([{"target": "h_a", "payload": {"type": "hop", "next": ["h_d"]}}])
    )[0]
    assert out["result"]["status"] == "failed", out
    assert out["result"]["got"]["reason"] == "unauthorized", out
    assert _hop_log == ["h_a"], "h_d's handler never ran"


async def test_a_peer_address_is_an_outbound_key_only(seeded_kernel):
    """A caller is always local, so a `<guid>|<agent>` key in `callable_by` could
    never match: refused at the write. In `can_call` it is an outbound peer address,
    accepted."""
    k = seeded_kernel
    _hop_chain(k)
    refused = (
        await k.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "update_agent",
                        "id": "h_d",
                        "callable_by": {"g2|scout": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert refused["reason"] == "invalid_meta", refused
    assert refused["error"] == (
        "callable_by['g2|scout'] is not a grant key — legal keys: an agent id, "
        "$self, $parent, $subtree"
    )
    ok = (
        await k.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "update_agent",
                        "id": "h_d",
                        "can_call": {"g2|scout": "*", "g3|*": "*"},
                    },
                }
            ]
        )
    )[0]["result"]
    assert ok["updated"] is True, ok
