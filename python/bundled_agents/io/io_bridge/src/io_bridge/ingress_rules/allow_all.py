"""`allow_all` — the OPEN ingress rule: full symmetric duplex, a true no-op. NOT the
default — IO legs seal by default (absent rule ⇒ `deny_inbound`); this is the rule you
set CONSCIOUSLY to open a leg (`ingress_rule=allow_all`)."""

from __future__ import annotations

from dataclasses import dataclass

from io_bridge._base import ALLOW, Action, Decision, IngressRule


@dataclass(frozen=True)
class AllowAll(IngressRule):
    """Permit every inbound action. The INTERIOR default (an opened or in-process leg)."""

    def authorize(self, action: Action) -> Decision:
        return ALLOW
