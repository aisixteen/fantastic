"""The ONE shape every refused call answers with.

Before this there were ~18 of them: `{"error": str}`, `{"error", "reason",
"side", "hint"}`, `{"error", "agent_count", "agents"}`, `{"error", "created":
False}`, `{"error", "updated": False}`, `{"error", "locked": True}`. A caller —
human, LLM, or another runtime — had to know which verb it had called to know
which shape came back, and a machine could not branch on the failure at all
without string-matching the prose.

So: one dict, one machine field, one human field.

    {"error":  "<one line a person reads>",
     "reason": "<code a machine branches on>",
     "hint":   "<what to do instead>",    # optional but strongly encouraged
     "see":    "<readme key>",            # optional doc pointer
     ...extra}                            # facts specific to this refusal

The machine field is `reason`, which ALREADY EXISTS — the call gate and every io
leg answer `reason:"unauthorized"`, and `relay_connector` answers its own
transport codes there. Nothing is renamed. What was actually missing is that most
refusals carried no machine field at all: `{"error": "create_agent:
handler_module required"}` could only be branched on by matching prose.

The set is CLOSED — a new code is a deliberate addition here, not an ad-hoc
string at a call site.

The `hint` is not decoration. A model that copies a literal placeholder `<id>` out
of a prompt example and gets back only `{"error": "no agent '<id>'"}` has a fact and
none of the facts that would repair it. A refusal
that cannot be acted on costs the same as no reply.
"""

from __future__ import annotations

from typing import Any

# The closed set. Each names a KIND of refusal, not a call site — two verbs that
# fail for the same reason share a code, and a caller branches on the code.
UNAUTHORIZED = "unauthorized"  # the call gate said no (carries `side`)
UNKNOWN_AGENT = "unknown_agent"  # no agent by that id (carries `agents`)
MISSING_ARG = "missing_arg"  # a required argument was absent (carries `arg`)
BAD_ARG = "bad_arg"  # present but the wrong type or value (carries `arg`)
INVALID_META = "invalid_meta"  # the bundle's own `validate_meta` refused
LOCKED = "locked"  # `delete_lock` (carries `id`)
NOT_HERE = "not_here"  # right verb, wrong agent — kernel verbs are root-only
EXISTS = "exists"  # an id that is already taken (carries `id`)
UNHANDLED = "unhandled"  # the agent has no such verb (carries `verb`)
NOT_FOUND = "not_found"  # the named THING is absent — a path, a job, a key
DRAINING = "draining"  # the kernel is stopping; no new roots are accepted
NO_CONSUMER = "no_consumer"  # watch: the watcher has no registered inbox
SECRET_FOUND = "secret_found"  # a secret scan found a key, a row or a path in an output
FAILED = "failed"  # the operation was valid but the system could not finish it (I/O)

# Transport codes — a leg that could not carry the call at all. Owned by
# `relay_connector` today; listed here because they share the field, and a caller
# branching on `reason` must be able to see the whole vocabulary in one place.
NOT_CONNECTED = "not_connected"
TRANSPORT_ERROR = "transport_error"
TRANSPORT_CLOSED = "transport_closed"
TIMEOUT = "timeout"

CODES = frozenset(
    {
        UNAUTHORIZED,
        UNKNOWN_AGENT,
        MISSING_ARG,
        BAD_ARG,
        INVALID_META,
        LOCKED,
        NOT_HERE,
        EXISTS,
        UNHANDLED,
        NOT_FOUND,
        DRAINING,
        NO_CONSUMER,
        SECRET_FOUND,
        FAILED,
        NOT_CONNECTED,
        TRANSPORT_ERROR,
        TRANSPORT_CLOSED,
        TIMEOUT,
    }
)


def refusal(
    code: str, error: str, *, hint: str = "", see: str = "", **extra: Any
) -> dict:
    """Build the one refusal shape. `code` must be one of `CODES`.

    An unknown code is a programming error and raises — the point of a closed set
    is that it cannot quietly grow at a call site."""
    if code not in CODES:
        raise ValueError(f"unknown refusal code {code!r}; one of {sorted(CODES)}")
    out: dict[str, Any] = {"error": error, "reason": code}
    if hint:
        out["hint"] = hint
    if see:
        out["see"] = see
    out.update(extra)
    return out


def missing(verb: str, arg: str, *, hint: str = "") -> dict:
    """`<verb>: <arg> required` — the commonest refusal, spelled once."""
    return refusal(MISSING_ARG, f"{verb}: {arg} required", hint=hint, arg=arg)


def bad(verb: str, arg: str, want: str, *, hint: str = "") -> dict:
    """Present but unusable — say what was wanted, not just that it was wrong."""
    return refusal(BAD_ARG, f"{verb}: {arg} must be {want}", hint=hint, arg=arg)


def guard_agent_id(verb: str, agent_id: object, arg: str = "id") -> dict | None:
    """Refuse an agent id that is not ONE SAFE PATH SEGMENT. Returns a refusal,
    or None when the id is fine.

    An agent id is not just a name — it BECOMES A DIRECTORY. `kernel_state`
    builds a record's on-disk home by joining the id onto the store root
    (`_resolve_path`), so an id carrying `..` or a separator walks out of the
    store and the write, or the `rmtree`, lands wherever it pointed.

    An agent the call gate admits to the root loader could send
    `{"type": "forget_record", "id": "../../important_project_dir"}` and
    recursively delete a directory OUTSIDE the store entirely — the call gate
    not bypassed, the id simply unvalidated.

    The same discipline applies to blob filenames (`_guard_blob_name`). It is
    enforced HERE
    — at the kernel, where ids are minted — so every consumer inherits it rather
    than each remembering.

    `arg` names the field when the segment is not an id: a record's
    `children_dir` becomes a directory too (`<agent>/<children_dir>/<child>`),
    so `create_agent` / `update_agent` hold it to the same rule.
    """
    if not isinstance(agent_id, str) or not agent_id:
        return bad(verb, arg, "a non-empty string")
    if "/" in agent_id or "\\" in agent_id:
        return bad(
            verb, arg, f"a single path segment (no separators); got {agent_id!r}"
        )
    if agent_id == "." or agent_id == ".." or ".." in agent_id:
        return bad(verb, arg, f"a name that is not a path traversal; got {agent_id!r}")
    if agent_id.startswith("."):
        return bad(verb, arg, f"a name that is not hidden; got {agent_id!r}")
    if agent_id.isdigit():
        # Message ids are plain integers; in prompt text an all-digit agent id
        # would read as one. Names are words.
        return bad(
            verb, arg, f"a name, not a number (message ids are ints); got {agent_id!r}"
        )
    if "|" in agent_id:
        # `|` qualifies a FOREIGN agent (`<guid>|<agent>`): a local id never
        # carries one, so a gate can tell local from foreign by the id alone.
        return bad(
            verb,
            arg,
            f"a local name without `|` (it marks a foreign `guid|agent`); got {agent_id!r}",
        )
    if agent_id == RESERVED_ID:
        # `kernel` names the operator: sends to it reach the root, and a memory
        # store serves the operator under it. An agent holding that id would BE
        # the operator to every store.
        return bad(verb, arg, f"a name other than the reserved {RESERVED_ID!r}")
    return None


# The one reserved agent id — the operator's name (send target, store identity).
RESERVED_ID = "kernel"
