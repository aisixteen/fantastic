"""io_bridge — unit tests for the rule registries (the inbound FILTER + outbound
DECORATOR) and spec resolution. The engine that drives a rule through a transport
lives in the bridge suites; here we test the rules + resolvers in isolation."""

from __future__ import annotations

from io_bridge import Action
from io_bridge._base import describe
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
    assert isinstance(resolve_ingress({"ingress_rule": ""}), DenyInbound)
    assert isinstance(resolve_ingress({"ingress_rule": None}), DenyInbound)


def test_resolve_ingress_string_and_object_forms():
    assert isinstance(resolve_ingress({"ingress_rule": "deny_inbound"}), DenyInbound)
    # object form — `type` names the rule; config keys ride alongside it
    assert isinstance(
        resolve_ingress({"ingress_rule": {"type": "deny_inbound"}}), DenyInbound
    )
    assert isinstance(
        resolve_ingress({"ingress_rule": {"type": "allow_all"}}), AllowAll
    )


def test_auth_shorthand_is_enforced_not_only_reported():
    """`auth` opens/closes BOTH directions, and `describe()` must not disagree.

    Was three silent defects at once. This test asserted the precedence with
    `{"ingress_rule": "allow_all", "ingress_rule": "deny_inbound"}` — one dict, one key,
    the first value discarded by Python before the call — so it asserted nothing about
    precedence, and `resolve_ingress` did not read `auth` at all. Meanwhile `describe()`
    did, so `auth:"allow_all"` reported `sealed:false` and denied every call. Assert the
    two agree, in both directions, or the same gap reopens silently.
    """
    assert isinstance(resolve_ingress({"auth": "allow_all"}), AllowAll)
    # `password` is the value the shorthand exists FOR — the only one meaningful in both
    # registries (`allow_all` is ingress's open, `silent` is egress's). Both directions
    # must pick it up, or a leg that reads as authenticated dials with no credential.
    assert isinstance(resolve_ingress({"auth": "password"}), IngressPassword)
    assert isinstance(resolve_egress({"auth": "password"}), EgressPassword)
    # the per-direction field WINS over the shorthand (two keys, not one)
    assert isinstance(
        resolve_ingress({"auth": "allow_all", "ingress_rule": "deny_inbound"}),
        DenyInbound,
    )
    # and what reflect reports is what the gate enforces
    posture = describe({"auth": "allow_all"})
    assert posture["sealed"] is False
    assert isinstance(resolve_ingress({"auth": "allow_all"}), AllowAll)
    sealed = describe({"auth": "allow_all", "ingress_rule": "deny_inbound"})
    assert sealed["sealed"] is True
    assert isinstance(
        resolve_ingress({"auth": "allow_all", "ingress_rule": "deny_inbound"}),
        DenyInbound,
    )


def test_resolve_ingress_unknown_type_seals_and_names_the_bad_rule():
    """An unresolvable rule SEALS — it does not raise.

    It used to raise `ValueError`, on the theory of failing the boot loudly. But
    resolution runs PER CALL, not at boot: an agent set
    `ingress_rule="password:kernel-internal-transcripts"`, `update_agent` accepted
    it, and the next call raised out of the handler as an unhandled task exception
    that killed the caller's chain with no reply. An unknown rule is not an
    authorization, so deny — and say which name was wrong, in every denial, so the
    operator is not left guessing."""
    rule = resolve_ingress({"ingress_rule": "password:kernel-internal-transcripts"})
    d = rule.authorize(Action("call", "t", "reflect", {"type": "reflect"}))
    assert d.allowed is False
    assert "password:kernel-internal-transcripts" in d.reason
    assert "allow_all" in d.hint and "password" in d.hint, "must teach the legal set"
    # A plain seal still reads as a plain seal — no phantom rule name.
    plain = resolve_ingress({}).authorize(Action("call", "t", "reflect", {}))
    assert plain.allowed is False and "unknown" not in plain.reason


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
    assert EgressPassword(env="MY_GROUP").credential() == "abc"
    monkeypatch.delenv("MY_GROUP", raising=False)
    assert EgressPassword(env="MY_GROUP").credential() is None  # unset ⇒ nothing


def test_resolve_egress_threads_env_and_defaults_silent():
    # the string form names the rule; `auth`'s symmetry is asserted in the auth test
    assert isinstance(resolve_egress({"egress_rule": "password"}), EgressPassword)
    # object form: `env` (the secret-env key) reaches the rule's own field
    assert (
        resolve_egress({"egress_rule": {"type": "password", "env": "MY_GROUP"}}).env
        == "MY_GROUP"
    )
    # An INGRESS-only name is not an egress instruction, and absent is not either: both
    # present nothing. (The second assertion below was a byte-identical copy of the one
    # above, under a comment claiming it covered a second spelling. It covered nothing —
    # same rot as the duplicate `ingress_rule` key this file used to carry.)
    assert isinstance(resolve_egress({"egress_rule": "deny_inbound"}), Silent)
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
    assert isinstance(a, IngressPassword) and a.env == "FANTASTIC_GROUP_TOKEN"
