"""egress_rules — the outbound-DECORATOR rule registry (the "upper importer").

Each egress rule is its own module here; this package registers them BY NAME in
`REGISTRY`. `forward` resolves a leg's rule via `resolve_egress(record)` and stamps
its `credential()` onto the outbound frame envelope. Inbound-only policy names
(`allow_all` / `deny_inbound`) map to `Silent` — they present nothing — so the legacy
`auth` shorthand stays consistent.
"""

from __future__ import annotations

from io_bridge._base import EgressRule, construct, parse_spec
from io_bridge.egress_rules.password import Password
from io_bridge.egress_rules.silent import Silent

# name → egress rule class. Inbound-only names resolve to Silent (present nothing).
REGISTRY: dict[str, type[EgressRule]] = {
    "silent": Silent,
    "allow_all": Silent,
    "deny_inbound": Silent,
    "password": Password,
}


def resolve_egress(record: dict) -> EgressRule:
    """Resolve the leg's EGRESS rule: `egress_rule` if the record carries it, else the
    legacy `auth` shorthand (so `auth:"password"` presents the group token). Absent ⇒
    `Silent` (present nothing). Unknown type ⇒ `ValueError`."""
    spec = record["egress_rule"] if "egress_rule" in record else record.get("auth")
    name, cfg = parse_spec(spec)
    if name is None:
        return Silent()
    cls = REGISTRY.get(name)
    if cls is None:
        raise ValueError(f"unknown egress rule type {name!r}")
    return construct(cls, cfg)


__all__ = ["REGISTRY", "Password", "Silent", "resolve_egress"]
