"""io_bridge base types — the authorization decision surface shared by every channel.

An IO edge is governed by two independent, typed rules (mirrored on the wire,
enforced on the RECEIVER): an INGRESS rule (the inbound FILTER) and an EGRESS rule
(the outbound DECORATOR). Both are resolved BY NAME from the `ingress_rules` /
`egress_rules` folder registries. This module owns only the decision contract — the
`Action` a peer asks of a leg, the `Decision` a rule returns, the two rule ABCs, and
the spec parsing/construction helpers the registries share. The CHANNEL model that
wraps a rule with a transport + modality + credential extractor lives in `channel`.

Full conceptual model (the two gotchas, discovery-through-denial, sealed edges) is in
`io_bridge/readme.md` — the keystone discovery artifact a readme-only LLM bootstraps
from.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass, fields, is_dataclass

# The authorization verdict is a SUBSTRATE concept — defined once in the kernel
# and imported UP here (bundle → kernel, the legal direction). The call gate and
# every io edge rule return the SAME `Decision`; there is no second dataclass.
from kernel._authz import ALLOW, Decision  # noqa: F401  (ALLOW re-exported here)


@dataclass(frozen=True)
class Action:
    """One inbound request the peer is asking this leg to perform locally."""

    kind: str  # "call" (gated) | "watch" | "state_subscribe" | "emit" | "open"
    target: str  # the local agent id the peer addressed
    verb: str  # payload["type"] — the verb requested (e.g. "reflect", "create_agent")
    payload: dict  # the inbound payload (dispatched verbatim — NO credential in it)
    token: str | None = None  # the frame-envelope credential, if the peer presented one


class IngressRule(ABC):
    """The inbound FILTER — decides whether the peer may perform an inbound action."""

    @abstractmethod
    def authorize(self, action: Action) -> Decision: ...


class EgressRule(ABC):
    """The outbound DECORATOR — the credential this leg PRESENTS on its own outbound
    `call`s (stamped on the frame envelope by `forward`). `None` ⇒ present nothing."""

    @abstractmethod
    def credential(self) -> str | None: ...


def parse_spec(spec) -> tuple[str | None, dict]:
    """Normalize a rule spec to `(type_name, config)`.

    Falsy ⇒ `(None, {})`. A bare string ⇒ `(name, {})`. An object ⇒
    `(type, {…config})`. `env` is the secret-env key. Unknown keys pass through and
    are tolerantly filtered to the rule's own fields by `construct`."""
    if not spec:
        return None, {}
    if isinstance(spec, str):
        return spec, {}
    name = spec.get("type")
    cfg: dict = {}
    for k, v in spec.items():
        if k == "type":
            continue
        cfg[k] = v
    return name, cfg


def construct(cls, cfg: dict):
    """Instantiate a rule dataclass, tolerantly filtering `cfg` to its own fields so
    an extra/unknown key can't crash the boot."""
    if is_dataclass(cls):
        allowed = {f.name for f in fields(cls)}
        return cls(**{k: v for k, v in cfg.items() if k in allowed})
    return cls()


def rule_name(spec, default: str) -> str:
    """The rule TYPE name for reflect — never surfaces the rule's config (env var
    names, etc.). Absent ⇒ the caller's `default` (ingress: `deny_inbound`, the SEAL —
    IO legs seal by default; egress: `silent`)."""
    name, _ = parse_spec(spec)
    return name or default


def _scoped(name: str, suffix: str, was_scoped: bool) -> str:
    """Report the SCOPED rule name whichever field it came from. A per-direction
    field is taken as written; an `auth` stem gets this manifold's suffix — the
    GATE's exact rule (`ingress_rules/__init__.py`), so `reflect` never shows a
    name that resolves differently when written back into `ingress_rule`."""
    if was_scoped:
        return name
    return (
        "silent_egress"
        if suffix == "_egress" and name in ("allow_all", "deny")
        else name + suffix
    )


def describe(record: dict) -> dict:
    """A leg's auth posture for `reflect` — the rule NAMES (not config) + whether the
    leg is sealed: `{ingress_rule, egress_rule, sealed}`. Keyed by the field you SET
    (read-key == write-key), so a readme-only LLM reads `ingress_rule: deny_ingress`
    and opens with `ingress_rule: allow_all_ingress` — a direct mirror, no translation."""
    ing_scoped = "ingress_rule" in record
    egr_scoped = "egress_rule" in record
    ing_spec = record["ingress_rule"] if ing_scoped else record.get("auth")
    egr_spec = record["egress_rule"] if egr_scoped else record.get("auth")
    # The name a caller READS is the name it must WRITE: a present stem is scoped
    # the way the gate scopes it; an ABSENT spec is the manifold's default, which
    # already carries its suffix (it used to be suffixed twice, 2026-09-16).
    ing_name = rule_name(ing_spec, "")
    egr_name = rule_name(egr_spec, "")
    ingress = _scoped(ing_name, "_ingress", ing_scoped) if ing_name else "deny_ingress"
    egress = _scoped(egr_name, "_egress", egr_scoped) if egr_name else "silent_egress"
    return {
        "ingress_rule": ingress,
        "egress_rule": egress,
        "sealed": ingress != "allow_all_ingress",
    }
