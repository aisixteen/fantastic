"""`password` — kernel-GROUP membership by a shared secret (egress side: PRESENT)."""

from __future__ import annotations

import os
from dataclasses import dataclass

from io_bridge._base import EgressRule


@dataclass(frozen=True)
class Password(EgressRule):
    """Present this leg's group token (read from `token_env`, default
    `FANTASTIC_GROUP_TOKEN`) on every outbound `call`, so a paired group member's
    ingress `password` rule accepts it. The symmetric mirror of
    `ingress_rules.password`. Presents nothing when the env var is unset/empty."""

    token_env: str = "FANTASTIC_GROUP_TOKEN"

    def credential(self) -> str | None:
        return os.environ.get(self.token_env) or None
