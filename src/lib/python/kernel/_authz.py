"""Call security — the substrate authorization primitive.

The model is **fully-gated, two-sided, from-above-only**:

- Every AGENT-originated send (`current_sender` is not None) is checked TWICE:
  **egress** — the SENDER's `can_call` must allow `(target, verb)` — AND
  **ingress** — the TARGET's `callable_by` must allow `(sender, verb)`. An edge
  exists only if BOTH ends grant it. The gate NEVER branches on a verb name:
  nothing is exempt, not even `reflect`.
- An **operator** send (`current_sender` is None: the CLI, cold bootstrap, a
  loader hydrating at boot, a test driver, or a verb completing its own side
  effect under `operator_context`) is the ORIGIN root of trust and never reaches
  this module — the gate skips it. That is the one trust boundary, not a carve-out.

Both `can_call` and `callable_by` are grant maps of the same shape,
`{ id | token : ["<verb>", …] | "*" }`. Tokens resolve relative to the grant's
HOLDER (the sender for `can_call`, the target for `callable_by`):

  - `$self`     → the holder's own id
  - `$parent`   → the holder's parent id
  - `$children` → the holder's direct children (set membership)
  - `$loader`   → the persistence root (rarely named; not in the baseline)

The baseline stamped + MERGED on every `create_agent`, on BOTH fields, is
`{$self:*, $parent:*, $children:*}` — reach/accept your own subtree and your
creator, nothing else. No `"*"` wildcard token.

**Authority flows from ABOVE only.** An agent's grants are set by its creator/the
operator at `create_agent`, or delegated by a parent (bounded to a subset of the
granter's own `can_call`). An agent can NEVER raise its own authority
(`update_agent` on its own `can_call`/`callable_by` is refused) — enforced at the
gate, not here.

PRIVILEGED verbs (`create_agent`/`delete_agent`/`update_agent`) are never covered
by a wildcard — only an explicit list entry names them — EXCEPT when the other end
is the holder itself (forking/acting on your OWN subtree is baseline).
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

# The baseline stamped on every created agent, on BOTH `can_call` (who I may call)
# and `callable_by` (who may call me): reach/accept your own subtree + your
# creator. Explicit grants in the create payload MERGE on top. No `$loader` (persist
# runs as operator bookkeeping), no `"*"` wildcard.
DEFAULT_CAN_CALL: dict[str, object] = {"$self": "*", "$parent": "*", "$children": "*"}
DEFAULT_CALLABLE_BY: dict[str, object] = {
    "$self": "*",
    "$parent": "*",
    "$children": "*",
}

# Structural graph mutation. PRIVILEGED: a wildcard never satisfies these — they
# must be named explicitly — so "talk to your parent" can never silently become
# "mutate the graph through your parent". (Except when acting on your own subtree,
# where the holder == the other end.)
# Verbs a WILDCARD never covers — they must be named explicitly, because a
# wildcard grant is a statement about REACH and these are statements about
# AUTHORITY or about BULK READS.
#
# The mutators are here because they change the tree. `state_subscribe` is here
# because its operand is the WHOLE tree: the stream carries every send's sender,
# target, verb and payload summary, for every agent. Without it, the structural
# baseline (`$parent: "*"`) silently handed the entire stream to every direct
# child of the root — which is how a leaf browser leg could tap it while holding
# no grant that names it (dataroom audit, 2026-08-27).
PRIVILEGED_VERBS: frozenset[str] = frozenset(
    {
        "create_agent",
        "delete_agent",
        "update_agent",
        "state_subscribe",
        # HYDRATION's pair — reading a conversation WHOLE and writing one into
        # an agent. `messages` is the grantable read (digests, no bodies); these
        # two are what a loader does from above, so a wildcard must never carry
        # them: only a grant that NAMES them opens either.
        "rows_read",
        "rows_write",
    }
)

# The remedy shown on any denial: authority descends, it is never self-raised.
_FROM_ABOVE = (
    "authority is granted from ABOVE — set at create_agent by your creator, or by "
    "the operator; a parent may delegate a subset of its own reach. You cannot "
    "grant it to yourself. Agents are deny-all by default (both to call and to be "
    "called)."
)


def _verb_allowed(grant, verb: str, privileged: bool) -> bool:
    """A grant value is `"*"` (any verb) or a list of verb names (which may itself
    contain `"*"`). A PRIVILEGED verb loses its wildcard cover — only an explicit
    list entry names it."""
    if privileged:
        return isinstance(grant, (list, tuple)) and verb in grant
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
    loader_id: str | None = None,
    enforce_privileged: bool = True,
) -> Decision:
    """One matching engine, used BOTH ways. `grant` is the holder's grant map
    (`can_call` for egress, `callable_by` for ingress); `other_id` is the id at the
    other end of the edge (the target for egress, the sender for ingress). Tokens
    resolve relative to the HOLDER. Absent/empty grant ⇒ deny (the default posture).

    `enforce_privileged` gates the wildcard-exclusion for PRIVILEGED verbs. It is
    an EGRESS concern only: the rule exists so a caller's broad `$parent:*` can
    never silently BECOME "mutate the graph through my parent" — it constrains
    who may REACH a mutation. On the INGRESS side the escalation is already blocked
    at egress, so a callee's `$children:*` ("I trust my own subtree") legitimately
    accepts a privileged verb; the gate passes `enforce_privileged=False` there.
    """
    grant = grant or {}
    if not isinstance(grant, dict):
        # A malformed grant (a bare "*" string reached a record before write-time
        # validation existed) is DENY, never a crash: on 2026-09-01 a str grant
        # raised `.items()` out of the gate mid-serve and a correctly-opened web
        # leg answered 500. The gate judges what is there; refusing shape is the
        # writer's job.
        return Decision(False)
    kids = holder_children or frozenset()
    # Privileged verbs lose wildcard cover UNLESS the other end is the holder
    # itself (acting on your own subtree is baseline, not escalation) — egress only.
    privileged = (
        enforce_privileged and verb in PRIVILEGED_VERBS and other_id != holder_id
    )
    for key, g in grant.items():
        if key == "$children":
            matched = other_id in kids
        elif key == "$self":
            matched = other_id == holder_id
        elif key == "$parent":
            matched = other_id == holder_parent
        elif key == "$loader":
            matched = other_id == loader_id
        elif key.startswith("$"):
            matched = False
        else:  # literal id
            matched = key == other_id
        if matched and _verb_allowed(g, verb, privileged):
            return ALLOW
    return Decision(False)
