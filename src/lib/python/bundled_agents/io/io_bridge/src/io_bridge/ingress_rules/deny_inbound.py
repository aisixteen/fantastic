"""`deny_inbound` — the SEAL: refuse every inbound action, and teach how to open.

A sealed edge denies ALL inbound kinds (`call` / `watch` / `state_subscribe` /
`emit` / `open`), not only `call` — a sealed leg leaks neither dispatch nor
telemetry. Every denial carries a teaching pointer (`hint` + `see`) so an automated
builder hitting the locked door learns to open it (set an `ingress_rule`, read the
io_bridge readme) instead of dead-ending. Discovery-through-denial is the design.
"""

from __future__ import annotations

from dataclasses import dataclass

from io_bridge._base import Action, Decision, IngressRule


@dataclass(frozen=True)
class DenyInbound(IngressRule):
    """Refuse every inbound action; the denial points at the readme that explains
    how to open the edge — so the locked door is a signpost, not a wall.

    `unknown` carries the rule name that could not be resolved. An edge configured
    with a rule nobody implements is NOT authorized — it seals, and the denial says
    which name was wrong instead of leaving the operator to guess."""

    unknown: str | None = None

    def authorize(self, action: Action) -> Decision:
        reason = (
            f"edge sealed — unknown ingress rule {self.unknown!r}"
            if self.unknown
            else "edge sealed by policy"
        )
        return Decision(
            False,
            reason,
            hint=(
                "this edge is sealed — open it: update_agent <id> ingress_rule=allow_all"
                " (or password); reflect readme=true on this agent for the channel model"
                + (
                    f". {self.unknown!r} is not a rule name — the legal values are"
                    " allow_all, deny_inbound, password"
                    if self.unknown
                    else ""
                )
            ),
            see="",
        )
