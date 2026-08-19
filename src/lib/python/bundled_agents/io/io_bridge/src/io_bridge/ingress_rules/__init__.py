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
REGISTRY: dict[str, type[IngressRule]] = {
    "allow_all": AllowAll,
    "deny_inbound": DenyInbound,
    "password": Password,
}


def resolve_ingress(record: dict) -> IngressRule:
    """Resolve the leg's INGRESS rule: `ingress_rule` if the record carries it, else
    edge denies until the operator opens it consciously (the deny-all design; the open
    interior is unaffected — only IO legs resolve a rule).

    UNKNOWN TYPE ⇒ `DenyInbound(unknown=<name>)` — seal, and name the bad rule in
    every denial. It used to raise `ValueError`. The intent was "fail the boot
    loudly", but resolution happens PER CALL, not at boot: on 2026-08-02 an agent
    set `ingress_rule="password:kernel-internal-transcripts"`, `update_agent`
    accepted it, and the next call to that agent raised out of the handler — an
    unhandled task exception that killed the caller's chain with no reply at all.
    An unresolvable rule is not an authorization, so the honest answer is to deny;
    raising mis-secures nothing but takes the whole dispatch down with it.

    `auth` is the both-directions shorthand and the per-direction field WINS over it.
    Reading it here is not a convenience: measured 2026-08-07, this function read only
    `ingress_rule` while `describe()` (the same package, what `reflect` reports) read
    `ingress_rule` else `auth`. So a leg carrying `auth:"allow_all"` reflected
    `{ingress_rule:"allow_all", sealed:false}` and denied every inbound call. Fail-CLOSED,
    so nothing was exposed — but the posture an operator reads has to be the posture the
    gate enforces. Rust (`fantastic-bridge/src/lib.rs:404`) and Swift
    (`Authorizer.swift:116`) already resolved it this way; python was the outlier."""
    spec = record["ingress_rule"] if "ingress_rule" in record else record.get("auth")
    name, cfg = parse_spec(spec)
    if name is None:
        return DenyInbound()
    cls = REGISTRY.get(name)
    if cls is None:
        return DenyInbound(unknown=name)
    return construct(cls, cfg)


__all__ = ["REGISTRY", "AllowAll", "DenyInbound", "Password", "resolve_ingress"]
