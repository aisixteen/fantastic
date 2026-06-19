"""`password` — kernel-GROUP membership by a shared secret (ingress side: CHECK)."""

from __future__ import annotations

import hmac
import os
from dataclasses import dataclass

from io_bridge._base import ALLOW, Action, Decision, IngressRule


@dataclass(frozen=True)
class Password(IngressRule):
    """Authorize an inbound `call` only if its envelope `auth_token` matches this
    leg's group token, read from an env var (`token_env`, default
    `FANTASTIC_GROUP_TOKEN`) so the secret never touches the portable `.fantastic`
    workdir. Fail-closed: an unset/empty env var refuses every inbound `call`.
    Constant-time compare. The egress mirror (`egress_rules.password`) PRESENTS the
    same token, so one group token in env makes a leg a full member."""

    token_env: str = "FANTASTIC_GROUP_TOKEN"

    def authorize(self, action: Action) -> Decision:
        # Gate EVERY inbound kind the choke point asks about — `call` (dispatch) AND
        # the data-bearing kinds `emit`/`watch`/`state_subscribe`. The web inbound
        # legs route those through this same gate, so a missing token must refuse
        # them too, else a "locked" leg leaks telemetry to a tokenless client.
        expected = os.environ.get(self.token_env) or None
        if expected is None:
            return Decision(False, f"group token unset ({self.token_env})")
        presented = (
            action.token
        )  # the frame-envelope token (NOT the dispatched payload)
        if isinstance(presented, str) and hmac.compare_digest(presented, expected):
            return ALLOW
        return Decision(False, "invalid or missing group token")
