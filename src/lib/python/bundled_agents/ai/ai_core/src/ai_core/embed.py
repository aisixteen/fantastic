"""Inline agent views in a chat (#339): `embed {id}` and `unembed {id}`. Port of the
Rust `embed.rs` (#340).

An embed is ONE appended store row, `{kind:"embed", content:<agent id>}` — it has no
`role`, so it is a row of the conversation's store but never a TURN: it does not enter
the model's prompt, projection or retrieval (`_load_history` keeps only rows with a chat
role). `history` merges the card rows back into its digests so a chat page redraws the
same cards, in the same places, after a reload or a restart. The store is append-only,
so `unembed` appends a tombstone row `{kind:"unembed", content:<id>}`; the cards a chat
shows are its embed rows since the last `clear` minus the tombstoned ones (`cards`). A
`context_clear` starts a new thread, so cards before it are not in the head — the same
as turns.

Neither verb creates, moves or deletes an agent. `embed` makes the same checks the
canvas's `place {backend_id}` makes: the agent exists, it has a `view` record, and the
page may call what the panel calls (`view.calls` plus `watch`, all granted to `user` in
the agent's `callable_by`); it names what is missing as `grants_missing`. The chat page
measures the panel it draws and tells the model when it overflowed.

`core` is imported lazily inside the functions: `core` builds its verb table from this
module, so a top-level import would be circular.
"""

from __future__ import annotations

import json
from typing import Any

from ai_core.constants import CHAT_CARD_MAX_HEIGHT

#: The row kind that puts a card in the chat.
EMBED_KIND = "embed"
#: The row kind that takes it out again.
UNEMBED_KIND = "unembed"

_MAKE_HINT = (
    "create_agent a host agent with view {kind:html, bytes_file:<panel file>, "
    "calls:[get_html, watch]} and callable_by {user:[get_html, watch]}, then embed it by id"
)


def cards(rows: list[dict]) -> list[str]:
    """The card rows of a conversation (full rows or `history` digests), as the agent ids
    that are on show, oldest first: an embed adds its agent once, an unembed removes it.
    The id is the row's `content` (a digest's `head`)."""
    out: list[str] = []
    for r in rows:
        kind = r.get("kind")
        if kind not in (EMBED_KIND, UNEMBED_KIND):
            continue
        cid = r.get("content")
        if cid is None:
            cid = r.get("head")
        if not isinstance(cid, str):
            continue
        out = [c for c in out if c != cid]
        if kind == EMBED_KIND:
            out.append(cid)
    return out


def is_card_row(row: dict) -> bool:
    """Is `row` a card row (embed or unembed)?"""
    return row.get("kind") in (EMBED_KIND, UNEMBED_KIND)


async def card_rows(self_id: str, kernel) -> list[dict]:
    """This agent's card rows in the head (since the last clear), oldest first."""
    from ai_core import core

    return [r for r in await core._refresh_head(self_id, kernel) if is_card_row(r)]


def _refusal(msg: str, reason: str, hint: str) -> dict:
    return {"error": msg, "reason": reason, "arg": "id", "hint": hint}


def _id_arg(verb: str, payload: dict) -> tuple[str | None, dict | None]:
    raw = payload.get("id")
    if isinstance(raw, str) and raw.strip():
        return raw.strip(), None
    return None, {
        "error": f"{verb}: `id` is required — the host agent's id",
        "reason": "bad_arg",
        "arg": "id",
    }


def _allows(grant: Any, verb: str) -> bool:
    """Does a `callable_by` grant value let `verb` through (`"*"`, or a list naming it)?"""
    if grant == "*":
        return True
    return isinstance(grant, list) and any(v in (verb, "*") for v in grant)


def _card_box() -> dict:
    return {"width": "100%", "max_height": CHAT_CARD_MAX_HEIGHT}


async def embed(id: str, payload: dict, kernel) -> dict:
    """Store a card row for host agent `payload["id"]`. See the module doc."""
    from ai_core import core

    target, err = _id_arg("embed", payload)
    if err is not None:
        return err
    if target == id:
        return _refusal(
            "embed: a chat cannot embed itself", "bad_arg", "embed another agent's id"
        )
    rec = kernel.get(target)
    if not isinstance(rec, dict):
        return _refusal(
            f"embed: no agent {target!r}",
            "not_found",
            "reflect the kernel for the agents there",
        )
    view = rec.get("view")
    if not isinstance(view, dict):
        return _refusal(
            f"embed: {target} has no `view` record — a default chat/terminal/stub view "
            "is not an embeddable panel",
            "no_view",
            _MAKE_HINT,
        )
    # the verbs the panel calls: its record's `calls`, plus `watch` (always received)
    calls = [c for c in (view.get("calls") or []) if isinstance(c, str)]
    if "watch" not in calls:
        calls.append("watch")
    user = (rec.get("callable_by") or {}).get("user")
    missing = [c for c in calls if not _allows(user, c)]
    if missing:
        r = _refusal(
            f"embed: {target}'s callable_by does not let the user call {missing}",
            "grants_missing",
            "the panel's calls will be refused. Grant them: update_agent "
            f'{{id: {target}, callable_by: {{"$self":"*", "user": '
            f"{json.dumps(calls, separators=(',', ':'))}}}}}",
        )
        r["grants_missing"] = {target: missing}
        return r
    live = cards(await card_rows(id, kernel))
    if target in live:
        return {"embedded": target, "already": True, "card": _card_box()}
    row_id = core._mint_row_id(id, kernel)
    await core._save_history(
        id, kernel, [{"id": row_id, "kind": EMBED_KIND, "content": target}]
    )
    return {
        "embedded": target,
        "row": row_id,
        "card": _card_box(),
        "view": view.get("kind"),
        "hint": (
            f"the card is {CHAT_CARD_MAX_HEIGHT} px tall at most and fills the chat column's "
            "width. If the chat page finds it overflowing or taller than that it tells you — "
            "then redo the panel. unembed {id} takes the card out; the agent stays."
        ),
    }


async def unembed(id: str, payload: dict, kernel) -> dict:
    """Take the card of agent `payload["id"]` out of the chat (a tombstone row). Works for a
    live and for a deleted agent; never reads or writes the agent."""
    from ai_core import core

    target, err = _id_arg("unembed", payload)
    if err is not None:
        return err
    if target not in cards(await card_rows(id, kernel)):
        return {"unembedded": target, "was_embedded": False}
    row_id = core._mint_row_id(id, kernel)
    await core._save_history(
        id, kernel, [{"id": row_id, "kind": UNEMBED_KIND, "content": target}]
    )
    return {"unembedded": target}
