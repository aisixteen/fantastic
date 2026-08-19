"""Call-security grant engine — pure unit tests for `kernel._authz.authorize_call`.

Deny-all is the law: an agent-originated send is permitted only by an explicit
grant on the sender's record. These tests exercise the policy in isolation (no
kernel, no dispatch); the gate wiring is tested in `test_call_security.py`.
"""

from __future__ import annotations

from kernel._authz import DEFAULT_CAN_CALL, Decision, authorize_call


def _sender(**over) -> dict:
    rec = {"id": "worker", "parent_id": "boss"}
    rec.update(over)
    return rec


# ─── deny-all default ───────────────────────────────────────────


def test_absent_grant_denies_everything():
    d = authorize_call(_sender(), "send", "peer")
    assert isinstance(d, Decision) and not d.allowed
    assert d.reason and d.hint  # a denial always teaches how to open it


def test_empty_grant_denies():
    assert not authorize_call(_sender(can_call={}), "send", "peer").allowed


# ─── relative tokens ────────────────────────────────────────────


def test_self_token_resolves_to_sender_id():
    rec = _sender(can_call={"$self": "*"})
    assert authorize_call(rec, "ping", "worker").allowed
    assert not authorize_call(rec, "ping", "boss").allowed


def test_parent_token_resolves_to_parent_id():
    rec = _sender(can_call={"$parent": "*"})
    assert authorize_call(rec, "ping", "boss").allowed
    assert not authorize_call(rec, "ping", "worker").allowed


def test_loader_token_resolves_to_passed_loader_id():
    rec = _sender(can_call={"$loader": "*"})
    assert authorize_call(rec, "load", "root_loader", loader_id="root_loader").allowed
    # no loader ancestor → the token resolves to None → no match
    assert not authorize_call(rec, "load", "root_loader", loader_id=None).allowed


def test_unix_default_reaches_self_parent_and_children():
    rec = _sender(can_call=dict(DEFAULT_CAN_CALL))
    kids = frozenset({"kid"})
    assert authorize_call(rec, "send", "worker", child_ids=kids).allowed  # $self
    assert authorize_call(rec, "send", "boss", child_ids=kids).allowed  # $parent
    assert authorize_call(rec, "send", "kid", child_ids=kids).allowed  # $children
    assert not authorize_call(rec, "send", "stranger", child_ids=kids).allowed


def test_children_token_matches_only_direct_children():
    rec = _sender(can_call={"$children": "*"})
    assert authorize_call(rec, "ping", "kid", child_ids=frozenset({"kid"})).allowed
    assert not authorize_call(rec, "ping", "kid", child_ids=frozenset()).allowed


def test_fork_into_own_subtree_is_not_privileged():
    # create_agent on $self (adding a child to your OWN subtree) is baseline —
    # the wildcard self-grant covers it.
    rec = _sender(can_call={"$self": "*"})
    assert authorize_call(rec, "create_agent", "worker").allowed
    # …but create_agent on the PARENT (mutating another agent) stays privileged.
    assert not authorize_call(rec, "create_agent", "boss").allowed


# ─── literal targets + per-verb grants ──────────────────────────


def test_literal_target_wildcard_verb():
    rec = _sender(can_call={"memory": "*"})
    assert authorize_call(rec, "set", "memory").allowed
    assert authorize_call(rec, "read", "memory").allowed
    assert not authorize_call(rec, "set", "other").allowed


def test_per_verb_list_grants_exactly_those_verbs():
    rec = _sender(can_call={"memory": ["read"]})
    assert authorize_call(rec, "read", "memory").allowed
    assert not authorize_call(rec, "set", "memory").allowed


def test_verb_wildcard_inside_list():
    rec = _sender(can_call={"memory": ["*"]})
    assert authorize_call(rec, "anything", "memory").allowed


def test_create_agent_denied_under_default_grant():
    # the whole point: a default-grant agent cannot mutate the graph — `kernel`
    # (the root) is neither $self nor $parent.
    rec = _sender(can_call=dict(DEFAULT_CAN_CALL))
    assert not authorize_call(rec, "create_agent", "kernel_state").allowed


def test_explicit_kernel_grant_opens_mutation():
    rec = _sender(can_call={"kernel_state": ["create_agent"]})
    assert authorize_call(rec, "create_agent", "kernel_state").allowed
    assert not authorize_call(rec, "delete_agent", "kernel_state").allowed
