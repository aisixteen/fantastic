"""`can` LINES, the one grant table (grant model v9).

A grant is a line in the CALLER's own record, in its `can` field: a target id
(or `$parent`) and one level, `call: [verbs]`, `update: [fields]` or
`create: {who, what}`. The target keeps no list of callers. Without a line an
agent reaches only its own tree, by position (`call` only); the parent too
needs a `$parent` line. Every ancestor whose tree does not hold the target
caps the call. Every gate denial is the unknown answer, byte-identical to the
answer for a missing id.
"""

from __future__ import annotations

#: The spawned kernel's root (`HostKernel.seed`).
ROOT = "kernel_state"

#: Every verb on the target's tree.
ALL = {"call": ["*"]}


def call(*verbs: str) -> dict:
    """A `call` line on the named verbs."""
    return {"call": list(verbs)}


#: A chat's line on its provider: complete, the token stream (`watch`, gated as
#: a verb) and the image limits (`capabilities`). `reflect` passes on any hold.
PROVIDER = call("complete", "watch", "capabilities")

#: A key user's line on the credential store.
GET = call("get")

#: `reflect` on the root, which `docs` folded into (`tier`, `verb`, `part`).
DOCS = {ROOT: call("reflect")}


def create(who: tuple[str, ...] | list[str] = ("*",), what: object = "*") -> dict:
    """A `create` line: agents whose id matches `who` and whose bundle is in
    `what` (`{bundle: {field: value | "*"}}`, or `*` for every bundle)."""
    return {"create": {"who": list(who), "what": what}}


def is_unknown(reply: object, aid: str) -> bool:
    """The kernel's unknown answer for `aid`: no side, no hint, no ids."""
    return (
        isinstance(reply, dict)
        and reply.get("reason") == "unknown_agent"
        and reply.get("error") == f"no agent {aid!r}"
        and not ({"side", "hint", "ids"} & set(reply))
    )


def runs(allowed: list[str], what: str = "") -> str:
    """A command runner's `description` naming its `allowed` patterns.

    A `call` line shows the identity keys and the verbs it holds, never a field:
    `allowed` is a field, so an AI holding `call` on a runner does not see it.
    `description` is an identity key, so the runner says there what it may run
    (and the AI gets no `update` line that would let it widen its own allowlist).
    """
    pats = ", ".join(f"`{p}`" for p in allowed)
    return f"{what + ' ' if what else ''}Runs only commands matching (regex over the whole command line): {pats}."
