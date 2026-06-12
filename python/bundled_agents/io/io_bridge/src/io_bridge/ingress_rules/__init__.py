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
    the legacy `auth` shorthand. Absent ⇒ `DenyInbound` — **sealed by default**: an IO
    edge denies until the operator opens it consciously (the deny-all design; the open
    interior is unaffected — only IO legs resolve a rule). Unknown type ⇒ `ValueError`
    (fail the boot loudly rather than silently mis-securing)."""
    spec = record["ingress_rule"] if "ingress_rule" in record else record.get("auth")
    name, cfg = parse_spec(spec)
    if name is None:
        return DenyInbound()
    cls = REGISTRY.get(name)
    if cls is None:
        raise ValueError(f"unknown ingress rule type {name!r}")
    return construct(cls, cfg)


__all__ = ["REGISTRY", "AllowAll", "DenyInbound", "Password", "resolve_ingress"]
