"""Call security — the substrate authorization primitive.

The model is MINIMAL, two-sided, explicit-grants-only (owner decision 2026-10-03):

- Every AGENT-originated send (`current_sender` is not None) passes iff
  **egress** — the SENDER's `can_call` grants `(target, verb)` — AND
  **ingress** — the TARGET's `callable_by` grants `(sender, verb)`. The gate never
  branches on a verb name: `"*"` covers EVERY verb.
  Kernel verbs answered by the root take the same two-sided check against the root.
- An **operator** send (`current_sender` is None: a send made outside every
  dispatch — the CLI, the cold bootstrap, a test driver, a transport handing an
  inbound frame to its leg) is the ORIGIN root of trust and never reaches this
  module — the gate skips it. Identity comes ONLY from dispatch: the sends a
  handler makes (and the tasks it spawns) are that agent's.

Both fields are grant maps of one shape, `{key: "*" | ["<verb>", …]}`. A key is:

  - a literal local agent id (never containing `|`)
  - `$self`     → the holder's own id
  - `$parent`   → the holder's parent id
  - `$children` → the holder's DIRECT children
  - `<guid>|<agent>` → one agent of a connected kernel — `can_call` only (an
    OUTBOUND peer address)
  - `<guid>|*`  → every agent of that one kernel — `can_call` only

Tokens resolve relative to the grant's HOLDER (the sender for `can_call`, the
target for `callable_by`). A caller is always a LOCAL agent: a call a bridge leg
serves is judged as the LEG, so a `<guid>|…` key in `callable_by` could never
match and is refused at the write.

The baseline stamped on every agent, on BOTH fields, is `{"$self": "*"}` —
nothing else. Every other edge is an explicit grant.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Decision:
    """The one authorization verdict. Shared by the call gate and every
    io_bridge edge rule (which imports this UP from the substrate)."""

    allowed: bool
    reason: str = ""
    # An optional teaching pointer: when a rule denies, it may name how to OPEN
    # the path, so discovery-through-denial is a guided hop, not a guessing game.
    hint: str = ""
    see: str = ""


ALLOW = Decision(True)

# The baseline stamped on every agent, on BOTH `can_call` (who I may call) and
# `callable_by` (who may call me): yourself, nothing else. Explicit grants in the
# create payload MERGE on top.
DEFAULT_CAN_CALL: dict[str, object] = {"$self": "*"}
DEFAULT_CALLABLE_BY: dict[str, object] = {"$self": "*"}

# The structural tokens — the only `$` keys a grant map may carry.
GRANT_TOKENS: frozenset[str] = frozenset({"$self", "$parent", "$children"})

# The legal-key lists, as the write-time shape refusal names them — per field:
# a caller is always local, so only `can_call` may name a peer kernel's agent.
LEGAL_GRANT_KEYS = {
    "callable_by": "an agent id, $self, $parent, $children",
    "can_call": "an agent id, $self, $parent, $children, <guid>|<agent> or <guid>|*",
}


def bad_grant_key(field: str, key: object) -> bool:
    """Is `key` NOT a legal key of grant `field`? Legal on both: a literal agent id
    (non-empty, no `$` prefix, no `|`), `$self`/`$parent`/`$children`. `can_call`
    also takes `<guid>|<agent>` or `<guid>|*` (exactly one `|`, both halves
    non-empty) — an outbound peer address; in `callable_by` such a key could
    never match a caller and is refused."""
    if not isinstance(key, str) or not key:
        return True
    if key.startswith("$"):
        return key not in GRANT_TOKENS
    if "|" in key:
        if field != "can_call":
            return True
        guid, _, agent = key.partition("|")
        return not guid or not agent or "|" in agent
    return key == "*"


# The remedy shown on any denial: authority descends, it is never self-raised.
_FROM_ABOVE = (
    "grants come from above — the agent's parent (update_agent on its direct "
    "child) or the operator writes can_call and callable_by; an agent never "
    "raises its own"
)


def _verb_allowed(grant, verb: str) -> bool:
    """A grant value is `"*"` (every verb) or a list of verb names (which may
    itself contain `"*"`)."""
    if grant == "*":
        return True
    if isinstance(grant, (list, tuple)):
        return verb in grant or "*" in grant
    return False


def authorize_call(
    grant: dict | None,
    verb: str,
    other_id: str,
    *,
    holder_id: str | None,
    holder_parent: str | None = None,
    holder_children: frozenset[str] | set[str] | None = None,
) -> Decision:
    """One matching engine, used BOTH ways. `grant` is the holder's grant map
    (`can_call` for egress, `callable_by` for ingress); `other_id` is the id at the
    other end of the edge (the target for egress, the sender for ingress). Tokens
    resolve relative to the HOLDER. Absent/empty grant ⇒ deny (the default posture).
    """
    grant = grant or {}
    if not isinstance(grant, dict):
        # A malformed grant is DENY, never a crash: the gate judges what is there;
        # refusing shape is the writer's job.
        return Decision(False)
    kids = holder_children or frozenset()
    foreign = "|" in other_id
    for key, g in grant.items():
        if key == "$self":
            matched = not foreign and other_id == holder_id
        elif key == "$parent":
            matched = not foreign and other_id == holder_parent
        elif key == "$children":
            matched = not foreign and other_id in kids
        elif key.startswith("$"):
            matched = False
        elif key.endswith("|*"):
            # every agent of ONE peer kernel: `<guid>|*` (a local id never carries
            # `|`, so this only ever matches an outbound peer target)
            matched = other_id.startswith(key[:-1])
        else:  # literal id (a local id, or a peer target `<guid>|<agent>`)
            matched = key == other_id
        if matched and _verb_allowed(g, verb):
            return ALLOW
    return Decision(False)
