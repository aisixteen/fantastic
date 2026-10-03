"""Call-security grant engine — pure unit tests for `kernel._authz.authorize_call`.

Deny-all is the law, two-sided: an edge exists only if BOTH the sender's
`can_call` (egress) and the target's `callable_by` (ingress) grant it. This
module exercises the ONE direction-agnostic matching engine in isolation (no
kernel, no dispatch); the two-sided gate wiring is tested in
`test_call_security.py`.

`authorize_call(grant, verb, other_id, *, holder_id, holder_parent,
holder_children, loader_id, enforce_privileged)` — `grant` is the HOLDER's grant
map; `other_id` is the id at the other end of the edge. Tokens resolve relative
to the holder.
"""

from __future__ import annotations

from kernel._authz import (
    DEFAULT_CALLABLE_BY,
    DEFAULT_CAN_CALL,
    Decision,
    authorize_call,
)

# The holder is "worker", child of "boss", with one child "kid".
_HOLDER = dict(
    holder_id="worker", holder_parent="boss", holder_children=frozenset({"kid"})
)


def _auth(grant, verb, other, **over):
    facts = dict(_HOLDER)
    facts.update(over)
    return authorize_call(grant, verb, other, **facts)


# ─── deny-all default ───────────────────────────────────────────


def test_absent_grant_denies_everything():
    d = _auth(None, "send", "peer")
    assert isinstance(d, Decision) and not d.allowed


def test_empty_grant_denies():
    assert not _auth({}, "send", "peer").allowed


# ─── relative tokens ────────────────────────────────────────────


def test_self_token_resolves_to_holder_id():
    assert _auth({"$self": "*"}, "ping", "worker").allowed
    assert not _auth({"$self": "*"}, "ping", "boss").allowed


def test_parent_token_resolves_to_parent_id():
    assert _auth({"$parent": "*"}, "ping", "boss").allowed
    assert not _auth({"$parent": "*"}, "ping", "worker").allowed


def test_loader_token_resolves_to_passed_loader_id():
    g = {"$loader": "*"}
    assert _auth(g, "load", "root_loader", loader_id="root_loader").allowed
    # no loader id → the token resolves to None → no match
    assert not _auth(g, "load", "root_loader", loader_id=None).allowed


def test_children_token_matches_only_direct_children():
    assert _auth({"$children": "*"}, "ping", "kid").allowed
    assert not _auth(
        {"$children": "*"}, "ping", "kid", holder_children=frozenset()
    ).allowed


def test_baseline_reaches_self_parent_and_children():
    for base in (DEFAULT_CAN_CALL, DEFAULT_CALLABLE_BY):
        assert _auth(dict(base), "send", "worker").allowed  # $self
        assert _auth(dict(base), "send", "boss").allowed  # $parent
        assert _auth(dict(base), "send", "kid").allowed  # $children
        assert not _auth(dict(base), "send", "stranger").allowed


# ─── privileged verbs (egress) ──────────────────────────────────


def test_fork_into_own_subtree_is_not_privileged():
    # create_agent on $self (adding a child to your OWN subtree) is baseline —
    # the wildcard self-grant covers it.
    assert _auth({"$self": "*"}, "create_agent", "worker").allowed
    # …but create_agent on the PARENT (mutating another agent) stays privileged.
    assert not _auth({"$self": "*"}, "create_agent", "boss").allowed


def test_baseline_cannot_mutate_the_root():
    # a default-grant agent cannot mutate the graph — the root is neither $self
    # nor $parent-with-explicit-privilege; the wildcard never covers create_agent.
    assert not _auth(dict(DEFAULT_CAN_CALL), "create_agent", "kernel_state").allowed


def test_explicit_grant_opens_exactly_the_named_privileged_verb():
    g = {"kernel_state": ["create_agent"]}
    assert _auth(g, "create_agent", "kernel_state").allowed
    assert not _auth(g, "delete_agent", "kernel_state").allowed


def test_the_baseline_never_reaches_the_kernel_controls():
    # Measured 2026-09-28: `$parent: "*"` let every direct child of the root stop
    # the kernel, kill ANY task and read the whole ledger. A structural token's
    # wildcard covers none of them.
    for verb in (
        "kernel_stop",
        "kernel_start",
        "shutdown_agents",
        "ps",
        "cancel",
        "ledger",
        "ack_deadletter",
        "persist_record",
        "forget_record",
        "load_tree",
        "allow",
        "disallow",
    ):
        assert not _auth(dict(DEFAULT_CAN_CALL), verb, "boss").allowed, verb
        assert _auth({"boss": [verb]}, verb, "boss").allowed, verb


def test_the_baseline_never_reaches_an_agents_lifecycle_or_routes():
    # Measured 2026-10-03 (audit BV-1): a web_ws/web_rest leg's `$parent:*` let any
    # client it admitted `stop` its web host for every client, or `unmount` a sealed
    # sibling's routes. By verb name, whatever the bundle: a structural token's
    # wildcard (`$parent`, `$children`) covers none of them; a NAMED verb or a
    # NAMED id's `*` does.
    for verb in ("stop", "mount", "unmount", "boot"):
        assert not _auth(dict(DEFAULT_CAN_CALL), verb, "boss").allowed, verb
        assert not _auth(dict(DEFAULT_CAN_CALL), verb, "kid").allowed, verb
        assert _auth({"$parent": [verb]}, verb, "boss").allowed, verb
        assert _auth({"boss": "*"}, verb, "boss").allowed, verb
        # ingress (`enforce_privileged=False`): `$children:*` still accepts it
        assert _auth({"$children": "*"}, verb, "kid", enforce_privileged=False).allowed


def test_watching_the_root_loses_a_structural_wildcard():
    # Audit 2026-10-03 (KV-2): `watch` whose src is the ROOT (`loader_id`) needs a
    # NAMED grant; every other `watch` is an ordinary verb.
    on_root = dict(loader_id="boss")
    assert not _auth(dict(DEFAULT_CAN_CALL), "watch", "boss", **on_root).allowed
    assert _auth({"boss": ["watch"]}, "watch", "boss", **on_root).allowed
    assert _auth({"boss": "*"}, "watch", "boss", **on_root).allowed
    assert _auth({"$parent": ["watch"]}, "watch", "boss", **on_root).allowed
    assert _auth(dict(DEFAULT_CAN_CALL), "watch", "kid", **on_root).allowed
    assert _auth(dict(DEFAULT_CAN_CALL), "watch", "boss").allowed  # not the root


def test_a_named_wildcard_covers_privileged_verbs():
    # `*` under a NAMED id is the operator's own statement — it covers
    # everything, privileged verbs included. Whoever writes it owns it.
    g = {"kernel_state": "*"}
    for verb in ("create_agent", "delete_agent", "kernel_stop", "cancel", "allow"):
        assert _auth(g, verb, "kernel_state").allowed, verb
    assert _auth({"kernel_state": ["*"]}, "update_agent", "kernel_state").allowed


def test_ingress_wildcard_accepts_privileged_verbs():
    # enforce_privileged=False (the ingress posture): a callee's `$children:*`
    # legitimately accepts a privileged verb — escalation is blocked at egress.
    assert not _auth(
        {"$children": "*"}, "create_agent", "kid"
    ).allowed  # egress default
    assert _auth(
        {"$children": "*"}, "create_agent", "kid", enforce_privileged=False
    ).allowed  # ingress


# ─── literal targets + per-verb grants ──────────────────────────


def test_literal_target_wildcard_verb():
    g = {"memory": "*"}
    assert _auth(g, "set", "memory").allowed
    assert _auth(g, "read", "memory").allowed
    assert not _auth(g, "set", "other").allowed


def test_per_verb_list_grants_exactly_those_verbs():
    g = {"memory": ["read"]}
    assert _auth(g, "read", "memory").allowed
    assert not _auth(g, "set", "memory").allowed


def test_verb_wildcard_inside_list():
    assert _auth({"memory": ["*"]}, "anything", "memory").allowed


# ─── malformed grant SHAPE: deny, never crash ───────────────────


def test_non_dict_grant_denies_without_raising():
    """A bare-string grant (`can_call:"*"`) reached records before write-time
    validation existed; the gate must judge it DENY, never raise `.items()`
    out of a dispatch (measured 2026-09-01, E21_plan: a correctly-opened web
    leg answered 500)."""
    for bad in ("*", ["*"], 7):
        d = _auth(bad, "send", "peer")
        assert isinstance(d, Decision) and not d.allowed


# ─── emit is gated like a send (audit 2026-09-28, a16) ───────────


async def test_emit_on_another_agent_needs_both_halves(seeded_kernel):
    from kernel import sender_context

    for aid, grants in (("em_open", {"em_target": ["emit"]}), ("em_closed", {})):
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "id": aid,
                        "can_call": grants,
                    },
                }
            ]
        )
    await seeded_kernel.send(
        [
            {
                "target": "kernel_state",
                "payload": {
                    "type": "create_agent",
                    "parent_id": "kernel_state",
                    "handler_module": "file_bridge.tools",
                    "id": "em_target",
                    "callable_by": {"em_open": ["emit"], "em_closed": ["emit"]},
                },
            }
        ]
    )
    inbox = seeded_kernel.ctx.ensure_inbox("em_target")
    with sender_context("em_closed"):
        await seeded_kernel.emit("em_target", {"type": "poke", "n": 1})
    assert inbox.empty(), "an emit with no egress grant must be dropped"
    with sender_context("em_open"):
        await seeded_kernel.emit("em_target", {"type": "poke", "n": 2})
    got = inbox.get_nowait()
    assert got == {"type": "poke", "n": 2}, got
    # emitting on your OWN inbox is how every bundle publishes — ungated
    with sender_context("em_closed"):
        await seeded_kernel.emit("em_closed", {"type": "self"})
