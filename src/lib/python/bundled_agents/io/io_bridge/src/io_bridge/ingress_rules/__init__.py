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
# never half-resolve across the two registries. Measured 2026-08-21: with bare names,
# `auth:"silent"` (a legitimate EGRESS policy) resolved ingress to
# `DenyInbound(unknown='silent')` — it SEALED the leg, recorded the unknown name, and
# said nothing; and `auth:"allow_all"` silently handed egress a `Silent` it never
# asked for. One unscoped name against two registries with different membership is a
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
    every denial. It used to raise `ValueError`. The intent was "fail the boot
    loudly", but resolution happens PER CALL, not at boot: on 2026-08-02 an agent
    set `ingress_rule="password_ingress:kernel-internal-transcripts"`, `update_agent`
    accepted it, and the next call to that agent raised out of the handler — an
    unhandled task exception that killed the caller's chain with no reply at all.
    An unresolvable rule is not an authorization, so the honest answer is to deny;
    raising mis-secures nothing but takes the whole dispatch down with it.

    `auth` is the both-directions shorthand and the per-direction field WINS over it.
    Reading it here is not a convenience: measured 2026-08-07, this function read only
    `ingress_rule` while `describe()` (the same package, what `reflect` reports) read
    `ingress_rule` else `auth`. So a leg carrying `auth:"allow_all"` reflected
    `{ingress_rule:"allow_all_ingress", sealed:false}` and denied every inbound call. Fail-CLOSED,
    so nothing was exposed — but the posture an operator reads has to be the posture the
    gate enforces. Rust (`fantastic-bridge/src/lib.rs:404`) and Swift
    (`Authorizer.swift:116`) already resolved it this way; python was the outlier."""
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


__all__ = ["REGISTRY", "AllowAll", "DenyInbound", "Password", "resolve_ingress"]
