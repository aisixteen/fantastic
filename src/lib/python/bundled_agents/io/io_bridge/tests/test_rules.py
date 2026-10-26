"""io_bridge — unit tests for the rule registries (the inbound FILTER + outbound
DECORATOR) and spec resolution. The engine that drives a rule through a transport
lives in the bridge suites; here we test the rules + resolvers in isolation."""

from __future__ import annotations

import pytest

from io_bridge import Action, gate_inbound
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


def test_every_inbound_kind_is_sealed():
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


def test_ingress_resolves_allow_and_deny():
    assert isinstance(resolve_ingress({"ingress_rule": "deny_ingress"}), DenyInbound)
    # object form — `type` names the rule; config keys ride alongside it
    assert isinstance(
        resolve_ingress({"ingress_rule": {"type": "deny_ingress"}}), DenyInbound
    )
    assert isinstance(
        resolve_ingress({"ingress_rule": {"type": "allow_all_ingress"}}), AllowAll
    )


def test_auth_shorthand_is_symmetric():
    """`auth` opens/closes BOTH directions, and `describe()` must not disagree.

    Was three silent defects at once. This test asserted the precedence with
    `{"ingress_rule": "allow_all_ingress", "ingress_rule": "deny_ingress"}` — one dict, one key,
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
        resolve_ingress({"auth": "allow_all", "ingress_rule": "deny_ingress"}),
        DenyInbound,
    )
    # and what reflect reports is what the gate enforces
    posture = describe({"auth": "allow_all"})
    assert posture["sealed"] is False
    assert isinstance(resolve_ingress({"auth": "allow_all"}), AllowAll)
    sealed = describe({"auth": "allow_all", "ingress_rule": "deny_ingress"})
    assert sealed["sealed"] is True
    assert isinstance(
        resolve_ingress({"auth": "allow_all", "ingress_rule": "deny_ingress"}),
        DenyInbound,
    )


def test_unknown_rule_names_the_legal_values():
    """An unresolvable rule SEALS — it does not raise.

    Raising would fail loudly only at boot, but resolution runs PER CALL: an
    unknown `ingress_rule` raising out of the handler would kill the caller's chain
    with no reply. An unknown rule is not an
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


@pytest.mark.parametrize(
    "spec, name",
    [
        (7, "7"),
        (["allow_all_ingress"], "['allow_all_ingress']"),
        ({"type": ["x"]}, "{'type': ['x']}"),
        ({"type": {"a": 1}}, "{'type': {'a': 1}}"),
        (
            {"type": "password_ingress", "env": 7},
            "{'type': 'password_ingress', 'env': 7}",
        ),
    ],
)
# twin: a_malformed_spec_is_named_by_its_python_repr
# twin: a_malformed_spec_seals_under_its_python_repr
def test_a_malformed_spec_seals_and_names_itself(spec, name):
    """A spec that is not a string or an object, or whose `type`/`env` is not a
    string, is named by its repr — a name no registry holds — so it SEALS like an
    unknown name — never a raise on every frame (`.get` on an int, a dict lookup
    with a list, `os.environ.get(7)`) that leaves a web_rest leg answering 500 and
    a bridge call with no reply at all."""
    rule = resolve_ingress({"ingress_rule": spec})
    assert isinstance(rule, DenyInbound) and rule.unknown == name
    d = gate_inbound(
        rule, "call", {"target": "t", "payload": {"type": "reflect"}, "auth_token": "x"}
    )
    assert d.allowed is False
    assert d.reason == f"edge sealed — unknown ingress rule {name!r}"
    assert describe({"ingress_rule": spec})["ingress_rule"] == name
    with pytest.raises(ValueError) as e:
        resolve_egress({"egress_rule": spec})
    assert str(e.value) == f"unknown egress rule type {name!r}"


def test_a_null_type_or_env_is_an_absent_one():
    """null is ABSENT inside a spec as it is for the whole spec: a null `type`
    is a plain seal, a null `env` is the default variable."""
    plain = resolve_ingress({"ingress_rule": {"type": None}})
    assert isinstance(plain, DenyInbound) and plain.unknown is None
    pw = resolve_ingress({"ingress_rule": {"type": "password_ingress", "env": None}})
    assert isinstance(pw, IngressPassword) and pw.env == "FANTASTIC_GROUP_TOKEN"


# ─── password rule, both sides (kernel-group shared secret) ─────


# twin: password_rule_gates_every_kind
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


def test_password_compares_utf8_bytes_so_any_token_is_a_plain_answer(
    monkeypatch,
):
    """The compare is over UTF-8 BYTES. On `str` `hmac.compare_digest` raises
    TypeError for any non-ASCII text, so a tokenless client sending `é` would get a
    500 / a dead dispatch instead of a denial, and a non-ASCII group token would
    refuse its own holders."""
    monkeypatch.setenv("FANTASTIC_GROUP_TOKEN", "s3cret")
    for odd in ("é", "s3cr€t", "\ud800", "été"):
        d = IngressPassword().authorize(_call(odd))
        assert not d.allowed and d.reason == "invalid or missing group token", odd
    monkeypatch.setenv("FANTASTIC_GROUP_TOKEN", "päss-wört")
    assert IngressPassword().authorize(_call("päss-wört")).allowed
    assert not IngressPassword().authorize(_call("pass-wort")).allowed


def test_ingress_password_fails_closed_when_token_unset(monkeypatch):
    monkeypatch.delenv("FANTASTIC_GROUP_TOKEN", raising=False)
    d = IngressPassword().authorize(_call("anything"))
    assert not d.allowed and "unset" in d.reason  # misconfig must not allow


def test_egress_password_presents_token(monkeypatch):
    monkeypatch.setenv("MY_GROUP", "abc")
    assert EgressPassword(env="MY_GROUP").credential() == "abc"
    monkeypatch.delenv("MY_GROUP", raising=False)
    assert EgressPassword(env="MY_GROUP").credential() is None  # unset ⇒ nothing


def test_egress_resolves_and_presents():
    # the string form names the rule; `auth`'s symmetry is asserted in the auth test
    assert isinstance(
        resolve_egress({"egress_rule": "password_egress"}), EgressPassword
    )
    # object form: `env` (the secret-env key) reaches the rule's own field
    assert (
        resolve_egress(
            {"egress_rule": {"type": "password_egress", "env": "MY_GROUP"}}
        ).env
        == "MY_GROUP"
    )
    # An INGRESS-only name is not an egress instruction, and absent is not either: both
    # present nothing.
    assert isinstance(resolve_egress({"egress_rule": "silent_egress"}), Silent)
    assert isinstance(resolve_egress({}), Silent)


def test_asymmetric_ingress_egress_via_engine():
    # a hub: refuse inbound, still present a group token outbound
    rec = {
        "ingress_rule": "deny_ingress",
        "egress_rule": {"type": "password_egress", "env": "MY_GROUP"},
    }
    assert isinstance(resolve_ingress(rec), DenyInbound)
    assert isinstance(resolve_egress(rec), EgressPassword)


def test_resolve_tolerates_unknown_config_keys():
    a = resolve_ingress({"ingress_rule": {"type": "password_ingress", "bogus": 1}})
    assert isinstance(a, IngressPassword) and a.env == "FANTASTIC_GROUP_TOKEN"


def test_a_name_from_the_other_manifold_is_loud_not_silent():
    """Rule names are MANIFOLD-SCOPED so one name can never half-resolve across the
    two registries.

    Unscoped, `auth:"silent"` — a legitimate EGRESS policy — would resolve ingress
    to `DenyInbound(unknown='silent')`, SEALING the leg while recording the unknown
    name and saying nothing; and `auth:"allow_all"` would hand egress a `Silent`
    nobody asked for. One unscoped name against two
    registries with different membership is a fallback, and this kernel has none.

    It still DENIES (an unresolvable rule is not an authorization), but it now names
    the manifold mistake instead of looking like an ordinary seal."""
    from io_bridge.ingress_rules import resolve_ingress

    d = resolve_ingress({"ingress_rule": "silent_egress"})
    assert getattr(d, "unknown", None) == "silent_egress"
    dec = d.authorize(Action("call", "x", "read", {}))
    assert dec.allowed is False
    assert "manifold-scoped" in dec.hint, "the denial must teach WHY the name failed"
    assert "allow_all_ingress" in dec.hint, "and name this manifold's legal values"

    # The `auth` stem is scoped per manifold, so an egress-only stem cannot silently
    # seal ingress: it surfaces as this manifold's missing rule, by name.
    assert resolve_ingress({"auth": "silent"}).unknown == "silent_ingress"

    # And the stems that DO span both manifolds still work from one field.
    assert resolve_ingress({"auth": "password"}).__class__.__name__ == "Password"
    assert resolve_ingress({"auth": "allow_all"}).__class__.__name__ == "AllowAll"
