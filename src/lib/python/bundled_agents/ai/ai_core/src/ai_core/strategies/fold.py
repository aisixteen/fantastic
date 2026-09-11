"""`fold` — self-inflicted, mechanical compaction that leaves marks the model can walk.

At the watermark, oldest first, one batch down to LOW:
  1. FOLD closed chains — one summarizing call per chain (title + body), one appended
     fold row each with `refs`; the members stay one descent away
  2. still over → STRIP bodies in open chains (the `strip` strategy, unchanged)
  3. still over → CUT (strip's own second stage)

THE ONE STRATEGY THAT WRITES. Every other strategy is a pure projection; this one
appends fold rows to the durable store, because a fold is a mark that must persist
— the next turn has to see the same collapse, or the prefix rewrites every turn.
Bookkeeping from above, same path as the chat store itself. Nothing is ever edited.

If the summarizer fails, the fold is still written — refs plus the members' joined
titles — and says so on its own line. Degraded, loud, never silent.
"""

from __future__ import annotations

from ai_core.context import ProjectionCtx, estimate_tokens
from ai_core.folds import (
    chain_text,
    is_fold,
    level,
    make_fold,
    render,
)
from ai_core.strategies.base import NOTICE_ENVELOPE_RESERVE, Projection
from ai_core.strategies.strip import LOW, strip

MAX_TITLE = 140


def _titles_of(rows: list[dict], chain: list[int]) -> str:
    import re

    out = []
    for i in chain:
        m = re.search(r'· "([^"]{1,140})"', rows[i].get("content") or "")
        if m:
            out.append(m.group(1))
    return "; ".join(dict.fromkeys(out)) or "(untitled)"


async def _summarize(
    ctx: ProjectionCtx, rows: list[dict], chain: list[int]
) -> tuple[str, str, bool]:
    """(title, body, degraded). One summarizer call for one closed chain."""
    if ctx.summarize is None:
        t = _titles_of(rows, chain)
        return t[:MAX_TITLE], t, True
    ask = {
        "role": "user",
        "content": (
            "FOLD this finished piece of work. First line: a one-sentence title (what it "
            "was and how it ended). Then a blank line. Then 2-5 sentences: what was asked, "
            "what was found or decided, anything that still binds. Output only that.\n\n"
            + chain_text(rows, chain)
        ),
    }
    try:
        text = (await ctx.summarize([ask]) or "").strip()
    except Exception:
        text = ""
    if not text:
        t = _titles_of(rows, chain)
        return t[:MAX_TITLE], t, True
    # The title is the first NON-EMPTY line; a leading blank line or a stray quote
    # must not become the title. Reasoning is already stripped by the summarizer; a
    # summary that still opens a think block is not a summary — degrade to titles.
    lines = [ln.strip() for ln in text.splitlines()]
    lines = [ln for ln in lines if ln]
    if not lines or lines[0].startswith("<think"):
        t = _titles_of(rows, chain)
        return t[:MAX_TITLE], t, True
    title = lines[0].strip('"').strip("*#").strip()
    body = "\n".join(lines[1:]).strip()
    return (title[:MAX_TITLE] or "(untitled)", body or title, False)


KEEP_TAIL = 6  # rows at the end of the view that never fold — the live tail
SPAN = 8  # rows per fold at the watermark — small enough to summarize, large enough to matter


async def write_fold(ctx: ProjectionCtx, rows: list[dict], idxs: list[int]) -> dict:
    """Summarize `idxs` and append ONE fold row for them. A fold row in `idxs` is just a
    member — that is how folds nest: the next fold over this stretch of the view takes
    the earlier fold as one of its rows, and its level is one above the deepest member."""
    from ai_core.core import _save_history  # lazy: core imports this package

    ids = [rows[i].get("id") for i in idxs]
    lvl = 1 + max((level(rows, rows[i]) for i in idxs if is_fold(rows[i])), default=0)
    title, text, degraded = await _summarize(ctx, rows, idxs)
    from ai_core.core import _ctx  # lazy: core imports this package

    if ctx.kernel is None:
        raise RuntimeError("fold: no kernel to mint the fold's id from")
    fr = make_fold(
        ctx.self_id,
        ids,
        title,
        text,
        degraded,
        id=_ctx(ctx.kernel).mint_id(),
        lvl=lvl,
        parent=rows[idxs[0]].get("parent"),
    )
    if ctx.client_id and ctx.kernel is not None:
        await _save_history(ctx.self_id, ctx.kernel, [fr])
    return fr


#: Adjacent folds at the head of the view before they fold as a group.
#:
#: THREE, not four. A real run folds at most once per turn — each projection
#: drops the view under the watermark and stops — so the head accumulates folds
#: one at a time and this threshold is the whole story. At four it never fired:
#: measured 2026-09-11, `nested_folds` and `flat_roots` both ended `levels [1]`,
#: a head that grows one fold per span forever, which is the growth this
#: constant exists to stop.
#:
#: Three still skips-then-batches, so depth grows as log₃ of the history rather
#: than one level per cut (the 2026-09-06 measurement: twelve levels in 26 cuts,
#: when every span began on the previous fold).
FOLD_RUN = 3


def next_span(
    rows: list[dict], view: list[dict], *, keep_tail: int = KEEP_TAIL, size: int = SPAN
) -> list[int]:
    """The oldest foldable stretch of the VIEW, as indices into `rows`.

    Never the goal turn, never the last `keep_tail` rows, and never ending between a
    call and its answer. Folds are ordinary members, but NOT the first pick: a stretch
    that starts on the previous fold wraps it, and doing that every time builds a linked
    list of summaries-of-summaries (measured 2026-09-06: twelve levels deep in 26 cuts,
    the model lost in it for 50 turns). So: skip the folds at the head and take the oldest
    NON-fold rows; only when `FOLD_RUN` folds sit adjacent at the head do they become the
    span themselves. Depth then grows with the log of the history, not its length."""
    by_id = {r.get("id"): i for i, r in enumerate(rows) if r.get("id")}
    head = (
        [r for r in view[1:-keep_tail] if r.get("id") in by_id]
        if len(view) > keep_tail + 1
        else []
    )
    lead = 0
    while lead < len(head) and is_fold(head[lead]):
        lead += 1
    if lead >= FOLD_RUN:
        take = head[:lead]  # fold the folds — same operation, same summarizer
    else:
        take = head[lead : lead + size]
        while take and take[-1].get("role") == "assistant":
            take = take[:-1]
    return [by_id[r["id"]] for r in take] if len(take) >= 2 else []


async def fold(
    body: list[dict], system_block: list[dict], rec: dict, ctx: ProjectionCtx
) -> Projection:
    rows = list(body)
    view = render(rows)
    budget = ctx.budget - NOTICE_ENVELOPE_RESERVE
    folded = 0
    if estimate_tokens(view) > budget and len(rows) > 2:
        target = int(budget * LOW)
        # ONE MOVE, REPEATED: fold the oldest stretch of the view — rows, folds, or a
        # mix — until under the low watermark or nothing foldable is left.
        while estimate_tokens(view) > target:
            span = next_span(rows, view)
            if not span:
                break
            rows.append(await write_fold(ctx, rows, span))
            folded += 1
            view = render(rows)
    # TIDY THE HEAD, budget or no budget. Coalescing used to happen only while
    # the view was OVER the watermark, so a projection that dropped under it left
    # its adjacent folds standing — and they stayed standing, because the next
    # projection folds ordinary rows first and only revisits the head when it is
    # over budget with `FOLD_RUN` folds already there. A session therefore grew
    # one fold per span at the head, forever.
    #
    # Terminates: each pass replaces `FOLD_RUN` adjacent folds with ONE, so the
    # run shrinks by `FOLD_RUN - 1` every time and `next_span` stops selecting
    # once fewer than `FOLD_RUN` remain.
    while True:
        span = next_span(rows, view)
        if not span or not all(is_fold(rows[i]) for i in span):
            break
        rows.append(await write_fold(ctx, rows, span))
        folded += 1
        view = render(rows)
    # Stage two and three: strip bodies in what is left, then cut — strip's own logic.
    out = await strip(view, system_block, rec, ctx)
    return Projection(
        body=out.body,
        omitted_marker=out.omitted_marker,
        stripped=out.stripped,
        stripped_bytes=out.stripped_bytes,
        folded=folded,
    )
