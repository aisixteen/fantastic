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


@dataclass(frozen=True)
class Action:
    """One inbound request the peer is asking this leg to perform locally."""

    kind: str  # "call" (gated) | "watch" | "state_subscribe" | "emit" | "open"
    target: str  # the local agent id the peer addressed
    verb: str  # payload["type"] — the verb requested (e.g. "reflect", "create_agent")
    payload: dict  # the inbound payload (dispatched verbatim — NO credential in it)
    token: str | None = None  # the frame-envelope credential, if the peer presented one


@dataclass(frozen=True)
class Decision:
    allowed: bool
    reason: str = ""
    # An optional teaching pointer: when a rule denies, it may name the readme that
    # explains how to OPEN this edge, so discovery-through-denial is a guided hop
    # rather than a guessing game. Surfaced on the wire as `{reason, hint, see}`.
    hint: str = ""
    see: str = ""


ALLOW = Decision(True)


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
    `(type|policy, {…config})` — the type comes from `type` (preferred) or the legacy
    `policy`; `env` is the canonical secret-env key (legacy `token_env` is accepted
    and folded to it). Unknown keys pass through and are tolerantly filtered to the
    rule's own fields by `construct`."""
    if not spec:
        return None, {}
    if isinstance(spec, str):
        return spec, {}
    name = spec.get("type") or spec.get("policy")
    cfg: dict = {}
    for k, v in spec.items():
        if k in ("type", "policy"):
            continue
        cfg["token_env" if k == "env" else k] = v  # `env` (new) → token_env field
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


def describe(record: dict) -> dict:
    """A leg's auth posture for `reflect` — the rule NAMES (not config) + whether the
    leg is sealed: `{ingress_rule, egress_rule, sealed}`. Keyed by the field you SET
    (read-key == write-key), so a readme-only LLM reads `ingress_rule: deny_inbound`
    and opens with `ingress_rule: allow_all` — a direct mirror, no translation."""
    ing_spec = (
        record["ingress_rule"] if "ingress_rule" in record else record.get("auth")
    )
    egr_spec = record["egress_rule"] if "egress_rule" in record else record.get("auth")
    ingress = rule_name(ing_spec, "deny_inbound")
    return {
        "ingress_rule": ingress,
        "egress_rule": rule_name(egr_spec, "silent"),
        "sealed": ingress != "allow_all",
    }
