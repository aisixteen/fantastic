"""THE RECORD SCHEMA of an AI backend kind — what its record takes, readable with
NO agent existing (owner, 2026-09-30, lib request #3).

A set-up page asks the catalog (`reflect bundles=all`: each entry carries the
bundle's `schema` when its module publishes `RECORD_SCHEMA`) and builds its inputs
from it; it guesses nothing. Per field: `name`, `type`
(`str|int|float|bool|enum|list|map`), `required`, `enum` (the legal values, for an
`enum`), `default` (THE BACKEND'S OWN value, present only where the backend has
one — a required field never has one: nothing is guessed), and a one-line `hint`.
Keys never ride the record: a backend that sends one has a `credentials` field
(type `credentials`, `{store, ref}` — the credential store and the ref it reads
the key from at each request, `ai_core/credentials.py`).

ONE TRUTH. The same schema is what the backend's `validate_meta` enforces AT THE
WRITE (`validate`): every required field is refused at `create_agent`/
`update_agent` when absent, and an `enum` field outside its values is refused —
so the page and the kernel cannot disagree about what a record needs. A field's
other checks (a positive int, a budget below `max_tokens`) stay in the backend's
own `validate_meta`, beside this one.

A backend's schema is its own fields followed by `AI_FIELDS` — the ones every
ai_core agent takes. One of them, `system_prompt`, is checked here too: named on a
write it must be a non-empty string (`null` clears it — the default block again).
"""

from __future__ import annotations

from ai_core.constants import (
    DEFAULT_CONTEXT_WINDOW,
    DEFAULT_OUTPUT_RESERVE,
    RECENT_N,
    RECENT_N_RANGE,
)
from ai_core.credentials import shape_error
from ai_core.strategies import DEFAULT_STRATEGY, strategy_names

TYPES = ("str", "int", "float", "bool", "enum", "list", "map", "credentials")


def field(
    name: str,
    type_: str,
    hint: str,
    *,
    required: bool = False,
    default=None,
    enum: tuple | list | None = None,
) -> dict:
    """One field. `default` is omitted when the backend has none (a required field
    never has one); `enum` is present exactly when `type_` is `enum`."""
    if type_ not in TYPES:
        raise ValueError(f"field {name!r}: type {type_!r} is not one of {TYPES}")
    if (type_ == "enum") != (enum is not None):
        raise ValueError(f"field {name!r}: `enum` values go with type 'enum', only")
    if required and default is not None:
        raise ValueError(f"field {name!r}: a required field has no default")
    out: dict = {"name": name, "type": type_, "required": required}
    if enum is not None:
        out["enum"] = list(enum)
    if default is not None:
        out["default"] = default
    out["hint"] = hint
    return out


#: The fields EVERY ai_core agent takes, whatever its backend.
AI_FIELDS: tuple = (
    field(
        "memory_store_id",
        "str",
        "the memory store agent that keeps this agent's conversation (e.g. a ram_store)",
        required=True,
    ),
    field(
        "context_window",
        "int",
        "tokens the model sees per turn",
        default=DEFAULT_CONTEXT_WINDOW,
    ),
    field(
        "output_reserve",
        "int",
        "tokens of the window held back for the model's reply",
        default=DEFAULT_OUTPUT_RESERVE,
    ),
    field(
        "context_strategy",
        "enum",
        "how the conversation is fitted into the window",
        enum=strategy_names(),
        default=DEFAULT_STRATEGY,
    ),
    field(
        "recent_n",
        "int",
        f"recent turns a compaction keeps verbatim ({RECENT_N_RANGE[0]}–{RECENT_N_RANGE[1]})",
        default=RECENT_N,
    ),
    field(
        "system_prompt",
        "str",
        "a system block that REPLACES the default one on every turn of this agent "
        "(a per-call `system_prompt` beats it for that delivery); null = the default",
    ),
)

#: The refusal for a `system_prompt` the record cannot use.
SYSTEM_PROMPT_REFUSAL = (
    "system_prompt must be a non-empty string (it replaces the default system "
    "block on every turn) — null clears it"
)


def credentials_field(*, required: bool) -> dict:
    """The `credentials` field of a backend that sends a key."""
    return field(
        "credentials",
        "credentials",
        "{store, ref}: the credential store and ref holding `api_key` — read at "
        "each request, sent only to the ref's host"
        + ("" if required else "; absent = no key (a keyless server)"),
        required=required,
    )


def schema(backend_fields) -> dict:
    """A backend's whole record schema: its own fields, then `AI_FIELDS`."""
    fields = [*backend_fields, *AI_FIELDS]
    names = [f["name"] for f in fields]
    if len(names) != len(set(names)):
        raise ValueError(f"duplicate field in schema: {names}")
    return {"fields": fields}


def _present(f: dict, v) -> bool:
    """Whether a record NAMES this field in a usable form — a string must be
    non-blank, anything else non-null."""
    if v is None:
        return False
    if f["type"] == "str":
        return isinstance(v, str) and bool(v.strip())
    return True


def validate(sch: dict, backend: str, meta: dict, record: dict) -> str | None:
    """THE WRITE-TIME CHECK the schema implies, on the MERGED record: an `enum`
    field the write names must be one of its values, a `system_prompt` it names a
    non-empty string (or null), and every required field must be present. Every
    missing field is named at once, each with its hint, so one retry fixes the
    record."""
    for f in sch["fields"]:
        v = meta.get(f["name"])
        if f["type"] == "enum" and v is not None and v not in f["enum"]:
            return (
                f"{f['name']}={v!r} is not one of the legal values: "
                f"{', '.join(map(str, f['enum']))}"
            )
        if f["type"] == "credentials" and v is not None and shape_error(v):
            return shape_error(v)
    sp = meta.get("system_prompt")
    if sp is not None and not (isinstance(sp, str) and sp.strip()):
        return SYSTEM_PROMPT_REFUSAL
    merged = {**record, **meta}
    missing = [
        f
        for f in sch["fields"]
        if f["required"] and not _present(f, merged.get(f["name"]))
    ]
    if missing:
        return (
            f"{backend}: required on the record and missing: "
            f"{', '.join(f['name'] for f in missing)} — nothing is guessed. "
            + "; ".join(f"{f['name']} = {f['hint']}" for f in missing)
        )
    return None
