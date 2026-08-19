"""Call security — the substrate authorization primitive.

Two facts live here, and the whole system shares them:

1. `Decision` — the ONE authorization verdict shape. Every gate in the system
   (the call gate below, and every io_bridge edge rule) returns THIS type.
   io_bridge imports it UP from the kernel; the kernel imports no bundle. So the
   verdict is defined once, in the substrate, where authorization belongs.

2. `authorize_call` — the CALL gate's policy: may a sender invoke `verb` on
   `target_id`? The model is **deny-all + egress grants**:

   - The kernel law for an AGENT-originated send (`sender != None`) is DENY.
     A send is permitted only by an explicit grant on the SENDER's record.
   - An OPERATOR send (`sender == None`: the CLI, the cold bootstrap, the loader
     at hydration, a test driver calling `kernel.send`) is the root of trust and
     never reaches this function — the gate skips it.

   The grant is `can_call: { target | token : ["<verb>", ...] | "*" }` on the
   sender's record. Keys are literal target ids OR relative tokens resolved at
   the gate:

     - `$self`     → the sender's own id          (talk to yourself)
     - `$parent`   → the sender's parent id        (talk to your creator)
     - `$children` → the sender's direct children  (command what you spawned)
     - `$loader`   → the sender's nearest loader ancestor

   The Unix default the kernel stamps on every created agent is
   `{"$self": "*", "$parent": "*", "$children": "*"}` — command your own subtree
   (yourself + the children you spawn) and answer your creator, nothing else.
   `$children` is load-bearing, not convenience: an agent can NEVER be allowed to
   grant ITSELF new edges (that would collapse deny-all), so the only way a
   runtime-spawned child (an AI backend's `chat` store, say) becomes reachable is
   a STRUCTURAL default — you may always talk to what you created. Because a
   loader is the parent of the roster it hydrates, `$parent` already reaches the
   loader; `$loader` only matters for grandchildren.

   Spawning is baseline too: `create_agent` on `$self` (adding a child to your
   OWN subtree) is not privileged — you may always fork. What stays privileged is
   structural mutation of ANOTHER agent (the escalation direction: up to the root
   or sideways to a peer).

   `reflect` is never gated (discovery moat) — the gate excludes it before
   calling here.
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
    # Surfaced on the wire as `{reason, hint, see}`.
    hint: str = ""
    see: str = ""


ALLOW = Decision(True)

# The Unix default stamped on every created agent: command your own subtree
# (self + the children you spawn), answer your creator, and PERSIST your own state
# through the loader — nothing else. Explicit `can_call` in a create payload
# overrides it. `$loader` is scoped to the two persistence verbs on purpose: every
# agent may save/load its own blobs through the loader (a baseline, like a process
# writing its own state dir), but NOT wield the loader's structural verbs
# (persist_record / forget_record / load_tree).
DEFAULT_CAN_CALL: dict[str, object] = {
    "$self": "*",
    "$parent": "*",
    "$children": "*",
    "$loader": ["persist_blob", "load_blob"],
}

# Structural graph mutation. These are PRIVILEGED: a blanket grant (`"*"`, or a
# list containing `"*"`) never satisfies them — they must be named explicitly,
# so "talk to your parent" (the Unix default, where $parent is the root for a
# depth-1 agent) can never silently become "mutate the graph through your
# parent". Unix parallel: rwx on a directory is not permission to chmod it.
PRIVILEGED_VERBS: frozenset[str] = frozenset(
    {"create_agent", "delete_agent", "update_agent"}
)


def resolve_tokens(sender_rec: dict, loader_id: str | None) -> dict[str, str | None]:
    """Map the SINGLE-id relative tokens to concrete ids for THIS sender.
    `$children` is a SET, handled separately in the match loop."""
    return {
        "$self": sender_rec.get("id"),
        "$parent": sender_rec.get("parent_id"),
        "$loader": loader_id,
    }


def _verb_allowed(grant, verb: str, privileged: bool) -> bool:
    """A grant value is `"*"` (any verb) or a list of verb names (which may
    itself contain `"*"`). A PRIVILEGED verb is the exception — a wildcard never
    covers it; only an explicit list entry does. `privileged` is passed in
    because create_agent on `$self` (forking your own subtree) is NOT privileged."""
    if privileged:
        return isinstance(grant, (list, tuple)) and verb in grant
    if grant == "*":
        return True
    if isinstance(grant, (list, tuple)):
        return verb in grant or "*" in grant
    return False


def authorize_call(
    sender_rec: dict,
    verb: str,
    target_id: str,
    loader_id: str | None = None,
    child_ids: frozenset[str] | set[str] | None = None,
) -> Decision:
    """Deny-all + egress grants. Pure — the gate resolves `loader_id` +
    `child_ids` from the tree and hands them in, so this stays unit-testable.

    Matches `(target_id, verb)` against the sender's `can_call`, resolving
    relative tokens. Absent/empty grant ⇒ deny (the default posture).
    """
    can_call = sender_rec.get("can_call") or {}
    tokens = resolve_tokens(sender_rec, loader_id)
    sender_id = sender_rec.get("id")
    kids = child_ids or frozenset()
    # A privileged (structural-mutation) verb loses its wildcard cover — UNLESS
    # the target is the sender itself: forking a child into your own subtree is
    # baseline authority, not escalation. Only mutating ANOTHER agent is gated.
    privileged = verb in PRIVILEGED_VERBS and target_id != sender_id
    for key, grant in can_call.items():
        if key == "$children":
            matched = target_id in kids
        elif key.startswith("$"):
            matched = tokens.get(key) == target_id
        else:  # literal id
            matched = key == target_id
        if not matched:
            continue
        if _verb_allowed(grant, verb, privileged):
            return ALLOW
    return Decision(
        False,
        reason=f"{sender_id} may not call {verb!r} on {target_id}",
        hint=(
            f"grant it on {sender_id}'s can_call — set it at create_agent, ask the "
            f"operator, or open your OWN reach: "
            f"send({sender_id}, {{type:'update_agent', id:'{sender_id}', "
            f"can_call:{{'{target_id}':['{verb}']}}}}) (use '*' for all verbs). "
            "Agents are deny-all by default."
        ),
    )
