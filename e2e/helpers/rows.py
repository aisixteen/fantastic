"""AN AGENT'S ROWS, REACHED THE SAME WAY ON EVERY RUNTIME — through its MEMORY STORE.

The kernel keeps no conversation. A probe that wants an agent's rows reads them
from the store as an admin naming the owner; one that PLANTS rows appends them
the same way. See `helpers/memory.py` for the wiring.

    rows_for   -> store `read`   {owner}          -> rows
    plant_rows -> store `append` {owner, rows}    -> ids

THE KERNEL IS THE ONLY ALLOCATOR. A planted row without an `id` takes the kernel's
next one (the store asks the counter); a probe that needs to cross-reference
planted rows (a fold's `refs`, a parent link) remaps them from the returned list —
see `plant_linked`.
"""

from __future__ import annotations

from typing import Any

from helpers import memory

#: Rows in an agent's store that are not conversation.
BOOKKEEPING = memory.BOOKKEEPING


async def rows_for(root: Any, aid: str, *, conversation_only: bool = False) -> list[dict]:
    """That agent's rows WHOLE, oldest first."""
    return await memory.rows_of(root, aid, conversation_only=conversation_only)


async def plant_rows(root: Any, aid: str, rows: list[dict]) -> list[int]:
    """Append `rows` to that agent's conversation. Returns the ids, in order."""
    return await memory.plant(root, aid, rows)


async def plant_linked(root: Any, aid: str, build) -> tuple[list[int], list[dict]]:
    """Plant rows that REFER TO EACH OTHER without ever minting an id.

    `build(ids)` is called twice: first with an empty list, to learn how many rows
    there are and to write the ones that need no cross-reference; then with the
    real ids, so a second pass can append the rows that name them (a fold's
    `refs`, a `parent`). Returns (all ids, the rows as finally written)."""
    first = build([])
    ids = await plant_rows(root, aid, first)
    second = build(ids)
    if second == first:
        return ids, first
    extra = second[len(first):]
    if extra:
        ids += await plant_rows(root, aid, extra)
    return ids, second
