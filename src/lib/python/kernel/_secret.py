"""SECRET MARKS on a send item.

An item may carry `secret: ["<dotted path>", ...]` — paths INTO its `payload`
(`"fields.api_key"`). The handler gets the payload as sent; everything the kernel
keeps or shows of the call — the call row in the store, the state stream's
`payload`/`summary`, the inbox mirrors to watchers — carries the payload AS
SHOWN: each marked value replaced by `SHOWN`. A path naming nothing is skipped;
`type` cannot be marked (the verb is routing, not data).

The marks are readable by the handler as `current_secret()`, so an agent that
relays the value onward (`user.forward`) marks its own send the same way.
"""

from __future__ import annotations

import copy

from kernel._refusal import BAD_ARG, refusal

SHOWN = "[secret]"


def check(item: dict) -> tuple[tuple[str, ...], dict | None]:
    """The item's marks as a tuple, or a `bad_arg` refusal for a malformed field.
    Absent ⇒ `()`."""
    marks = item.get("secret")
    if marks is None:
        return (), None
    if not isinstance(marks, list) or not all(
        isinstance(m, str) and m and "" not in m.split(".") for m in marks
    ):
        return (), refusal(
            BAD_ARG,
            "send: secret must be a list of dotted paths into the payload "
            f'(e.g. ["fields.api_key"]), got {marks!r}',
            arg="secret",
        )
    if "type" in marks:
        return (), refusal(
            BAD_ARG,
            "send: secret may not mark `type` — the verb is not data",
            arg="secret",
        )
    return tuple(marks), None


def shown(payload: dict, marks: tuple[str, ...]) -> dict:
    """`payload` with every marked value replaced by `SHOWN`; the input is never
    mutated. No marks ⇒ the payload itself."""
    if not marks:
        return payload
    out = copy.deepcopy(payload)
    for mark in marks:
        node = out
        *head, leaf = mark.split(".")
        for key in head:
            node = node.get(key) if isinstance(node, dict) else None
            if node is None:
                break
        if isinstance(node, dict) and leaf in node:
            node[leaf] = SHOWN
    return out
