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
`{ id | token : ["<verb>", …] | "*" }`. An id is a local agent id (never
containing `|`) or a FOREIGN agent `<guid>|<agent>` — a call relayed by a bridge
leg pinned to that peer; `<guid>|*` names every agent of that one kernel. A
foreign caller is judged on the TARGET's `callable_by` by its qualified id; the
structural tokens never match it, so the baseline gives foreign agents nothing. Tokens resolve relative to the grant's
HOLDER (the sender for `can_call`, the target for `callable_by`):

  - `$self`     → the holder's own id
  - `$parent`   → the holder's parent id
  - `$children` → the holder's direct children (set membership)
  - `$loader`   → the persistence root (rarely named; not in the baseline)
  - `$subtree:<X>` → X or any LIVE descendant of X, resolved at call time
    (callable_by ONLY — a target accepting a whole subtree that is spawned
    dynamically, so nobody has to name each member; never a foreign id)

A `can_call` value may also be a HERITABLE entry,
`{<target id>: {"verbs": [...], "for": "descendants"}}`: it reaches for the
holder's DESCENDANTS, never for the holder itself — the egress half for agents
spawned under an owner that must not hold the reach itself. The egress check
walks the sender's ancestors for one when the sender's own `can_call` refuses.

The baseline stamped + MERGED on every `create_agent`, on BOTH fields, is
`{$self:*, $parent:*, $children:*}` — reach/accept your own subtree and your
creator, nothing else. No `"*"` wildcard token.

**Authority flows from ABOVE only.** An agent's grants are set by its creator/the
operator at `create_agent`, or delegated by a parent (bounded to a subset of the
granter's own `can_call`). An agent can NEVER raise its own authority
(`update_agent` on its own `can_call`/`callable_by` is refused) — enforced at the
gate, not here.

PRIVILEGED verbs (the tree mutators, the bulk reads, the kernel's lifecycle and
task controls, the loader's record verbs) are never covered by a wildcard under a
STRUCTURAL token (`$self`/`$parent`/`$children`/`$loader`/`$subtree:<X>`) or in a
heritable entry — EXCEPT when the other end is the holder itself (acting on your
OWN subtree is baseline). A NAMED verb covers one. A `*` under a NAMED id is the
operator's own statement and covers everything, privileged verbs included: whoever
writes it owns it.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable
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

# Verbs a STRUCTURAL wildcard never covers (`$parent: "*"` and the other tokens —
# the baseline the kernel stamps on every agent). Under a token they must be named
# explicitly, because the baseline is a statement about REACH and these are
# statements about AUTHORITY or about BULK READS: "talk to your parent" can never
# silently become "mutate the graph / stop the kernel / kill any task through your
# parent". (Except when acting on your own subtree, where the holder == the other
# end.) A `*` under a NAMED id covers them — that grant is the operator's own.
#
# The mutators are here because they change the tree. `state_subscribe` is here
# because its operand is the WHOLE tree: the stream carries every send's sender,
# target, verb and payload summary, for every agent. Without it, the structural
# baseline (`$parent: "*"`) silently handed the entire stream to every direct
# child of the root — which is how a leaf browser leg could tap it while holding
# no grant that names it (production audit, 2026-08-27).
PRIVILEGED_VERBS: frozenset[str] = frozenset(
    {
        "create_agent",
        "delete_agent",
        "update_agent",
        "state_subscribe",
        # A file_bridge's EXPOSURE: `allow` widens what the bridge serves,
        # `disallow` narrows it. Under the baseline alone any parent or child
        # could `allow .` and reopen the deny-all (audit 2026-09-28).
        "allow",
        "disallow",
        # The kernel's LIFECYCLE and TASK CONTROLS, answered by the root. Under the
        # baseline alone, every direct child of the root could stop the kernel or
        # kill ANY task (audit 2026-09-28). `ledger` is the bulk read of every
        # execution; `ack_deadletter` retires one.
        "kernel_start",
        "kernel_stop",
        "shutdown_agents",
        "ps",
        "cancel",
        "ledger",
        "ack_deadletter",
        # The LOADER's record verbs — they write, delete and read the persisted
        # tree, grants included.
        "persist_record",
        "forget_record",
        "load_tree",
    }
)

# The structural tokens — the baseline's vocabulary. A privileged verb loses a
# wildcard's cover only under one of these (and under a `$subtree:<X>` key).
STRUCTURAL_TOKENS: frozenset[str] = frozenset(
    {"$self", "$parent", "$children", "$loader"}
)

# `$subtree:<X>` — X or any live descendant of X. callable_by ONLY.
SUBTREE_PREFIX = "$subtree:"

# A heritable `can_call` entry's one audience.
HERITABLE_FOR = "descendants"


def is_heritable(value: object) -> bool:
    """A heritable `can_call` entry: exactly `{"verbs": [...], "for": "descendants"}`."""
    return (
        isinstance(value, dict)
        and set(value) == {"verbs", "for"}
        and value.get("for") == HERITABLE_FOR
        and isinstance(value.get("verbs"), list)
        and bool(value["verbs"])
        and all(isinstance(v, str) for v in value["verbs"])
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
    if isinstance(grant, dict):
        return False  # a heritable entry never grants its HOLDER
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
    in_subtree: Callable[[str, str], bool] | None = None,
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

    `in_subtree(other_id, x)` answers "is other_id x or a live descendant of x" —
    the caller's live tree, asked at call time; absent ⇒ `$subtree:` keys match
    nothing.
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
    # Privileged verbs lose a STRUCTURAL wildcard's cover UNLESS the other end is
    # the holder itself (acting on your own subtree is baseline, not escalation) —
    # egress only. A named id's `*` keeps its cover (checked per key below).
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
        elif key.startswith(SUBTREE_PREFIX):
            x = key[len(SUBTREE_PREFIX) :]
            matched = (
                in_subtree is not None
                and bool(x)
                and "|" not in other_id
                and in_subtree(other_id, x)
            )
        elif key.startswith("$"):
            matched = False
        elif key.endswith("|*"):
            # every agent of ONE foreign kernel: `<guid>|*` (a local id never
            # carries `|`, so this can only ever match a foreign caller)
            matched = other_id.startswith(key[:-1])
        else:  # literal id (a local id, or a foreign `<guid>|<agent>`)
            matched = key == other_id
        structural = key in STRUCTURAL_TOKENS or key.startswith(SUBTREE_PREFIX)
        if matched and _verb_allowed(g, verb, privileged and structural):
            return ALLOW
    return Decision(False)


def authorize_heritable(
    ancestor_grants: Iterable[dict], verb: str, target_id: str
) -> Decision:
    """The heritable half of EGRESS: may a sender call `target_id` through a
    heritable `can_call` entry one of its ANCESTORS holds? `ancestor_grants` is
    their `can_call` maps, nearest first. A NAMED verb covers a privileged one;
    `*` covers only an ordinary verb. First allow wins."""
    for grant in ancestor_grants:
        entry = (grant or {}).get(target_id) if isinstance(grant, dict) else None
        if not is_heritable(entry):
            continue
        verbs = entry["verbs"]
        if verb in verbs or ("*" in verbs and verb not in PRIVILEGED_VERBS):
            return ALLOW
    return Decision(False)
