"""egress_rules — the outbound-DECORATOR rule registry (the "upper importer").

Each egress rule is its own module here; this package registers them BY NAME in
`REGISTRY`. `forward` resolves a leg's rule via `resolve_egress(record)` and stamps
its `credential()` onto the outbound frame envelope. Inbound-only policy names
(`allow_all` / `deny_inbound`) map to `Silent` — they present nothing.
"""

from __future__ import annotations

from io_bridge._base import EgressRule, construct, parse_spec
from io_bridge.egress_rules.password import Password
from io_bridge.egress_rules.silent import Silent

# name → egress rule class. Inbound-only names resolve to Silent (present nothing).
# MANIFOLD-SCOPED NAMES — see the note in `ingress_rules`. `allow_all`/`deny` are
# INBOUND policies; an inbound policy presents no outbound credential, so the `auth`
# stem for either resolves here to `Silent`. That is a MEANING, not a fallback: it is
# spelled out per stem rather than reached when a lookup misses.
REGISTRY: dict[str, type[EgressRule]] = {
    "silent_egress": Silent,
    "password_egress": Password,
}
# `auth` stems whose meaning on THIS manifold is "present nothing".
_INBOUND_ONLY_STEMS = {"allow_all", "deny"}
_STEM = "_egress"


def resolve_egress(record: dict) -> EgressRule:
    """Resolve the leg's EGRESS rule: `egress_rule` if the record carries it, else the
    `auth` both-directions shorthand, else `Silent` (present nothing). Unknown type ⇒
    `ValueError`.

    The `auth` leg of that precedence matches `describe()`, `resolve_ingress` and both
    other host runtimes; without it `auth:"password"` reflected as `egress_rule:password_egress`
    and then stamped no credential, so a leg that read as authenticated dialled bare."""
    scoped = "egress_rule" in record
    spec = record["egress_rule"] if scoped else record.get("auth")
    name, cfg = parse_spec(spec)
    if name is None:
        return Silent()
    if not scoped:
        if name in _INBOUND_ONLY_STEMS:
            return Silent()
        name = name + _STEM
    cls = REGISTRY.get(name)
    if cls is None:
        raise ValueError(f"unknown egress rule type {name!r}")
    return construct(cls, cfg)


__all__ = ["REGISTRY", "Password", "Silent", "resolve_egress"]
