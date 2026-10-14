"""ingress_rules — the inbound-FILTER rule registry (the "upper importer").

Each ingress rule is its own module here; this package registers them BY NAME in
`REGISTRY`. The read loop / web face resolves a leg's rule via `resolve_ingress(record)`
and consults it at the single inbound choke point. Add a rule = drop a module + one
`REGISTRY` line; the engine never changes.
"""

from __future__ import annotations

from io_bridge._base import IngressRule, construct, parse_spec
from io_bridge.ingress_rules.allow_all import AllowAll
from io_bridge.ingress_rules.deny_inbound import DenyInbound
from io_bridge.ingress_rules.password import Password

# name → ingress rule class. New ingress rule types register HERE.
# MANIFOLD-SCOPED NAMES. A rule name binds to exactly one direction, so a name can
# never half-resolve across the two registries. With bare names, `auth:"silent"` (a
# legitimate EGRESS policy) would resolve ingress to `DenyInbound(unknown='silent')` —
# SEALING the leg, recording the unknown name, and saying nothing; and
# `auth:"allow_all"` would silently hand egress a `Silent` it never asked for. One unscoped name against two registries with different membership is a
# fallback, and this kernel does not have fallbacks.
REGISTRY: dict[str, type[IngressRule]] = {
    "allow_all_ingress": AllowAll,
    "deny_ingress": DenyInbound,
    "password_ingress": Password,
}
# The `auth` both-directions shorthand is written as a bare STEM (`password`), and
# each manifold appends its own suffix. A stem with no rule in this manifold is an
# error, not a default — that is the whole point of the scoping.
_STEM = "_ingress"


def resolve_ingress(record: dict) -> IngressRule:
    """Resolve the leg's INGRESS rule: `ingress_rule` if the record carries it, else
    edge denies until the operator opens it consciously (the deny-all design; the open
    interior is unaffected — only IO legs resolve a rule).

    UNKNOWN TYPE ⇒ `DenyInbound(unknown=<name>)` — seal, and name the bad rule in
    every denial. Raising would fail loudly only at boot, but resolution happens PER
    CALL: an unknown rule raising out of the handler is an unhandled task exception
    that kills the caller's chain with no reply at all.
    An unresolvable rule is not an authorization, so the honest answer is to deny;
    raising mis-secures nothing but takes the whole dispatch down with it.

    `auth` is the both-directions shorthand and the per-direction field WINS over it.
    Reading it here is not a convenience: `describe()` (the same package, what `reflect`
    reports) reads `ingress_rule` else `auth`, and the posture an operator reads has to
    be the posture the gate enforces."""
    scoped = "ingress_rule" in record
    spec = record["ingress_rule"] if scoped else record.get("auth")
    name, cfg = parse_spec(spec)
    if name is None:
        return DenyInbound()
    if not scoped:  # `auth` carries a bare stem; this manifold names its own rule
        name = name + _STEM
    cls = REGISTRY.get(name)
    if cls is None:
        return DenyInbound(unknown=name)
    return construct(cls, cfg)


def unknown_ingress_refusal(meta: dict, legal: tuple = tuple(REGISTRY)) -> str | None:
    """THE write-time check every leg's `validate_meta` makes: an `ingress_rule`
    this registry does not implement (an unknown name, or a malformed spec) is
    refused at the WRITE, next to the typo. The gate would seal on it anyway, but
    `{"updated": true}` for a control that does not exist is its own failure.
    `legal` is the leg's own list — an in-process edge (fs/process) has no wire
    envelope to carry a token, so it names no `password_ingress`. Only a PRESENT
    `ingress_rule` is judged: the `auth` stem's ingress half may legitimately
    name no rule here."""
    if "ingress_rule" not in meta:
        return None
    spec = meta["ingress_rule"]
    if not getattr(resolve_ingress({"ingress_rule": spec}), "unknown", None):
        return None
    return (
        f"ingress_rule={spec!r} is not a rule this bundle implements — "
        f"legal values: {', '.join(legal)}"
    )


__all__ = [
    "REGISTRY",
    "AllowAll",
    "DenyInbound",
    "Password",
    "resolve_ingress",
    "unknown_ingress_refusal",
]
