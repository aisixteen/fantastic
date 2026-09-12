"""THE ROW STORE, REACHED THE SAME WAY ON EVERY RUNTIME.

A probe that wants an agent's conversation used to take it off the in-process
python kernel (`root.ctx.rows_for(aid)`), and one that wanted to PLANT rows
minted its ids from `root.ctx.mint_id()`. Neither exists across a socket: on a
spawned kernel `root` is a `WireRoot`, and the whole probe died in its scorer
with `'WireRoot' object has no attribute 'ctx'` — after the run had already
finished and paid for itself (measured 2026-09-11: `subai` and `subai_graph`,
27 minutes each, both lost at the last step).

So these go through the kernel's own verbs, which every runtime answers:

    rows_for   -> `rows_read`   {agent}            -> {rows, next}
    plant_rows -> `rows_write`  {agent, rows}      -> {ids}

Both are PRIVILEGED — the operator reaches them, a wildcard grant never does —
and both are exactly what a loader does from above, which is what a probe
planting a history IS.

THE STORE IS THE ONLY ALLOCATOR. `plant_rows` does not mint: it hands the kernel
rows WITHOUT ids and gets back the ids it assigned, in order. A probe that needs
to cross-reference planted rows (a fold's `refs`, a parent link) remaps them from
that list — see `plant_linked`.
"""

from __future__ import annotations

from typing import Any

#: Kernel bookkeeping — in the same table, never the conversation.
BOOKKEEPING = ("call", "task", "body", "view", "settle")


async def _kernel(root: Any, payload: dict) -> dict:
    """One kernel verb, through whatever `root` is. `send` is the ONE shape the
    in-process root and `WireRoot` share."""
    out = await root.send([{"target": "kernel", "payload": payload}])
    return out[0].get("result") or {}


async def rows_for(root: Any, aid: str, *, conversation_only: bool = False) -> list[dict]:
    """That agent's rows WHOLE, oldest first. `conversation_only` drops the
    kernel's own bookkeeping kinds — what a reader means by "the conversation"."""
    r = await _kernel(root, {"type": "rows_read", "agent": aid})
    rows = r.get("rows")
    if not isinstance(rows, list):
        raise AssertionError(f"rows_read({aid}) refused: {r}")
    if conversation_only:
        rows = [x for x in rows if x.get("kind") not in BOOKKEEPING]
    return rows


async def plant_rows(root: Any, aid: str, rows: list[dict]) -> list[int]:
    """Append `rows` to that agent's conversation, each under a FRESH id.
    Returns the ids the store assigned, in the order given."""
    r = await _kernel(root, {"type": "rows_write", "agent": aid, "rows": rows})
    ids = r.get("ids")
    if not isinstance(ids, list):
        raise AssertionError(f"rows_write({aid}) refused: {r}")
    return ids


async def plant_linked(root: Any, aid: str, build) -> tuple[list[int], list[dict]]:
    """Plant rows that REFER TO EACH OTHER without ever minting an id.

    `build(ids)` is called twice: first with an empty list, to learn how many
    rows there are and to write the ones that need no cross-reference; then with
    the real ids, so a second pass can append the rows that name them (a fold's
    `refs`, a `parent`). Returns (all ids, the rows as finally written).

    Two passes rather than one because the store is the only allocator: a probe
    cannot know an id until the row exists, and inventing one is how a planted
    history stops matching the counter the kernel hands the model.
    """
    first = build([])
    ids = await plant_rows(root, aid, first)
    second = build(ids)
    if second == first:
        return ids, first
    extra = second[len(first):]
    if extra:
        ids += await plant_rows(root, aid, extra)
    return ids, second


async def body_get(root: Any, pid: int, offset: int = 0, length: int = 4096) -> dict | None:
    """One page of a stored reply body, by pid — through the OWNER's `pull`,
    which is the verb that exists on every runtime. `None` when nothing is
    stored under that pid."""
    r = await root.send([{"target": "kernel", "payload": {"type": "messages", "ids": [pid]}}])
    got = (r[0].get("result") or {}).get("rows")
    if not got:
        return None
    return {"pid": pid, "row": got[0], "offset": offset, "length": length}
