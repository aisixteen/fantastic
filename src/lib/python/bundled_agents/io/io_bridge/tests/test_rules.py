"""io_bridge — unit tests for the rule registries (the inbound FILTER + outbound
DECORATOR) and spec resolution. The engine that drives a rule through a transport
lives in the bridge suites; here we test the rules + resolvers in isolation."""

from __future__ import annotations

import pytest

from io_bridge import Action
from io_bridge.egress_rules import Silent, resolve_egress
from io_bridge.egress_rules.password import Password as EgressPassword
from io_bridge.ingress_rules import AllowAll, DenyInbound, resolve_ingress
from io_bridge.ingress_rules.password import Password as IngressPassword


def _call(token=None):
    # the credential rides the frame envelope (Action.token), NOT the dispatched payload
    return Action("call", "t", "reflect", {"type": "reflect"}, token=token)


# ─── ingress rules (the inbound FILTER) ─────────────────────────


def test_allow_all_permits_call_and_watch():
    a = AllowAll()
    assert a.authorize(_call()).allowed
    assert a.authorize(Action("watch", "t", "watch", {})).allowed


def test_deny_inbound_seals_all_kinds_and_teaches():
    a = DenyInbound()
    # the SEAL refuses every inbound kind — dispatch AND telemetry
    for kind in ("call", "watch", "state_subscribe", "emit", "open"):
        d = a.authorize(Action(kind, "t", kind, {}))
        assert not d.allowed and d.reason
    # …and every denial teaches how to open it (discovery-through-denial)
    d = a.authorize(_call())
    assert d.see == "" and "ingress_rule" in d.hint


def test_resolve_ingress_absent_is_deny_inbound():
    assert isinstance(
        resolve_ingress({}), DenyInbound
    )  # sealed by default — the seal is the lift's default
    assert isinstance(resolve_ingress({"auth": ""}), DenyInbound)
    assert isinstance(resolve_ingress({"ingress_rule": None}), DenyInbound)


def test_resolve_ingress_string_and_object_forms():
    assert isinstance(resolve_ingress({"auth": "deny_inbound"}), DenyInbound)
    # the symmetric per-direction field overrides the `auth` shorthand
    assert isinstance(
        resolve_ingress({"auth": "allow_all", "ingress_rule": "deny_inbound"}),
        DenyInbound,
    )
    # object form, both `type` (new) and `policy` (legacy) spellings
    assert isinstance(
        resolve_ingress({"ingress_rule": {"type": "deny_inbound"}}), DenyInbound
    )
    assert isinstance(
        resolve_ingress({"auth": {"policy": "deny_inbound"}}), DenyInbound
    )


def test_resolve_ingress_unknown_type_raises():
    with pytest.raises(ValueError):
        resolve_ingress({"ingress_rule": "nope"})


# ─── password rule, both sides (kernel-group shared secret) ─────


def test_ingress_password_checks_envelope_token(monkeypatch):
    monkeypatch.setenv("FANTASTIC_GROUP_TOKEN", "s3cret")
    p = IngressPassword()
    assert p.authorize(_call("s3cret")).allowed
    assert not p.authorize(_call("nope")).allowed
    assert not p.authorize(_call(None)).allowed  # no auth_token at all
    # password now gates the data-bearing kinds too (not just call): watch /
    # state_subscribe require the token, else a "locked" leg leaks telemetry.
    assert not p.authorize(Action("watch", "t", "watch", {})).allowed
    assert p.authorize(Action("watch", "t", "watch", {}, token="s3cret")).allowed
    assert not p.authorize(Action("state_subscribe", "t", "ss", {})).allowed


def test_ingress_password_fails_closed_when_token_unset(monkeypatch):
    monkeypatch.delenv("FANTASTIC_GROUP_TOKEN", raising=False)
    d = IngressPassword().authorize(_call("anything"))
    assert not d.allowed and "unset" in d.reason  # misconfig must not allow


def test_egress_password_presents_token(monkeypatch):
    monkeypatch.setenv("MY_GROUP", "abc")
    assert EgressPassword(token_env="MY_GROUP").credential() == "abc"
    monkeypatch.delenv("MY_GROUP", raising=False)
    assert EgressPassword(token_env="MY_GROUP").credential() is None  # unset ⇒ nothing


def test_resolve_egress_threads_env_and_defaults_silent():
    # `auth:"password"` is symmetric: egress presents (the legacy shorthand)
    assert isinstance(resolve_egress({"auth": "password"}), EgressPassword)
    # `env` (new) and `token_env` (legacy) both reach the rule's field
    assert (
        resolve_egress(
            {"egress_rule": {"type": "password", "env": "MY_GROUP"}}
        ).token_env
        == "MY_GROUP"
    )
    assert (
        resolve_egress(
            {"auth": {"policy": "password", "token_env": "MY_GROUP"}}
        ).token_env
        == "MY_GROUP"
    )
    # inbound-only names + absent ⇒ Silent (present nothing) — back-compat wire shape
    assert isinstance(resolve_egress({"auth": "deny_inbound"}), Silent)
    assert isinstance(resolve_egress({}), Silent)


def test_asymmetric_ingress_egress():
    # a hub: refuse inbound, still present a group token outbound
    rec = {
        "ingress_rule": "deny_inbound",
        "egress_rule": {"type": "password", "env": "MY_GROUP"},
    }
    assert isinstance(resolve_ingress(rec), DenyInbound)
    assert isinstance(resolve_egress(rec), EgressPassword)


def test_resolve_tolerates_unknown_config_keys():
    a = resolve_ingress({"ingress_rule": {"type": "password", "bogus": 1}})
    assert isinstance(a, IngressPassword) and a.token_env == "FANTASTIC_GROUP_TOKEN"
