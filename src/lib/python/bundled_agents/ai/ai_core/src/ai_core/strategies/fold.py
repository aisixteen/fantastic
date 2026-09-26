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

import re

from ai_core.context import ProjectionCtx, estimate_one, estimate_tokens
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


# THE SUMMARY BUDGET — this strategy's own defaults (the per-agent strategy slot is
# where they become configurable; not exposed now). A summary may take a fifth of
# the span it replaces, and never more than a quarter of the body budget: a fold
# that is itself a large fraction of the window buys nothing.
SUMMARY_SHARE = 0.20
WINDOW_SHARE = 0.25
SUMMARY_FLOOR = 48  # tokens — a title and one sentence always fit
# THE THINKING ALLOWANCE on a summary call's `max_tokens` (decided 2026-09-26). The
# endpoint counts a reasoning model's thought against `max_tokens`; at a bare budget
# (as small as ~93 tokens for a short span) the thought alone hit the bound, every
# small summary was cut and refused, and the last-resort cut dropped the fold that held
# the run's decision (measured, E81). The SUMMARY TEXT — thought stripped — must
# still fit the budget; the allowance is for the thinking only.
THINK_ALLOWANCE = 1024

FOLD_PROMPT = (
    "FOLD this finished piece of work in AT MOST ~{budget} tokens. First line: a "
    "one-sentence title (what it was and how it ended). Then a blank line. Then these "
    "sections, each one line or a few, in this order, omitting none (write `none`):\n"
    "ASKED: what was asked.\n"
    "DONE: what was found or decided — AND WHY.\n"
    "BINDING: rules, constraints and choices that still bind.\n"
    "ERRORS: every error, quoted verbatim.\n"
    "OPEN: what is unfinished.\n"
    "If the piece contains earlier fold summaries, MERGE them — on a conflict the newer "
    "statement wins, and unfinished work is carried forward. Output only that.\n\n"
)
RESUMMARIZE = (
    "Your summary below is ~{got} tokens; the budget is {budget}. Rewrite it to fit — "
    "keep the title line, the reasons, the binding rules and every quoted error; drop "
    "narration. Output only the summary.\n\n"
)
RESUMMARIZE_CUT = (
    "Your summary below was CUT OFF at the budget ({budget} tokens) before it ended. "
    "Write it again, shorter, so it ENDS within the budget — keep the title line, the "
    "reasons, the binding rules and every quoted error; drop narration. Output only "
    "the summary.\n\n"
)


_PATH = re.compile(
    r"[\w./-]+\.(?:txt|md|py|json|ya?ml|js|ts|rs|swift|toml|csv|log|sh|html|css)\b"
)
_IDENT = re.compile(r"\b[0-9a-f]{8,}\b")
_RUNTIME = (
    "[reply to ",
    "[no call",
    "[context-notice",
    "[fold ",
    "[recall",
    "[late reply",
    "[cleared ",
    "[fork of ",
)
INDEX_USER_CHARS = 1200  # the user's own words, verbatim, at most this much per fold


def mechanical_index(rows: list[dict], chain: list[int]) -> str:
    """What a fold must never lose, extracted WITHOUT the model: the files and the
    identifiers its members name, and what the USER said, verbatim (newest kept first
    when over the cap). Appended to the summary — no summariser can drop it."""
    text = "\n".join(str(rows[i].get("content", "")) for i in chain)
    files = sorted(set(_PATH.findall(text)))[:30]
    idents = sorted(set(_IDENT.findall(text)))[:20]
    said: list[str] = []
    used = 0
    for i in reversed(chain):
        r = rows[i]
        c = str(r.get("content", "")).strip()
        if r.get("role") != "user" or r.get("kind") or not c or c.startswith(_RUNTIME):
            continue
        c = " ".join(c.split())
        if used + len(c) > INDEX_USER_CHARS:
            c = c[: max(0, INDEX_USER_CHARS - used)] + " …"
        said.insert(0, c)
        used += len(c)
        if used >= INDEX_USER_CHARS:
            break
    lines = []
    if files:
        lines.append("files: " + ", ".join(files))
    if idents:
        lines.append("identifiers: " + ", ".join(idents))
    lines += [f"user said: {u}" for u in said]
    return ("INDEX (mechanical):\n" + "\n".join(lines)) if lines else ""


def summary_budget(ctx: ProjectionCtx, rows: list[dict], chain: list[int]) -> int:
    span = estimate_tokens([rows[i] for i in chain])
    return max(
        SUMMARY_FLOOR, min(int(span * SUMMARY_SHARE), int(ctx.budget * WINDOW_SHARE))
    )


def _split(text: str) -> tuple[str, str] | None:
    lines = [ln.strip() for ln in text.splitlines()]
    lines = [ln for ln in lines if ln]
    if not lines or lines[0].startswith("<think"):
        return None
    title = lines[0].strip('"').strip("*#").strip()
    body = "\n".join(lines[1:]).strip()
    return title[:MAX_TITLE] or "(untitled)", body or title


async def _summarize(
    ctx: ProjectionCtx, rows: list[dict], chain: list[int], reserve: int = 0
) -> tuple[str, str, bool] | None:
    """(title, body, degraded), or None when the summary stays OVER ITS BUDGET (or is
    CUT OFF by it) after one re-summarise — then nothing is folded (the span stays in
    view; loud in `context_status`), never a truncated summary.

    The budget is stated in the prompt; the call's `max_tokens` is the budget plus
    `THINK_ALLOWANCE` (a reasoning model's thought counts against it); `reserve`
    is what the fold row spends on its mechanical index, so summary + index fit. A
    provider that stopped at `max_tokens` says so (`last_cut` on the summarizer) —
    a cut-off summary is treated like an oversized one, never kept."""
    if ctx.summarize is None:
        t = _titles_of(rows, chain)
        return t[:MAX_TITLE], t, True
    budget = max(SUMMARY_FLOOR, summary_budget(ctx, rows, chain) - reserve)

    async def ask(content: str) -> tuple[str, bool]:
        try:
            text = (
                await ctx.summarize(
                    [{"role": "user", "content": content}],
                    max_tokens=budget + THINK_ALLOWANCE,
                )
                or ""
            ).strip()
        except Exception:
            return "", False
        return text, bool(getattr(ctx.summarize, "last_cut", False))

    text, cut = await ask(FOLD_PROMPT.format(budget=budget) + chain_text(rows, chain))
    if not text:
        t = _titles_of(rows, chain)
        return t[:MAX_TITLE], t, True
    got = estimate_one({"role": "user", "content": text})
    if cut or got > budget:
        # ONE re-summarise, telling the model the numbers.
        tell = (
            RESUMMARIZE_CUT.format(budget=budget)
            if cut
            else RESUMMARIZE.format(got=got, budget=budget)
        )
        text, cut = await ask(tell + text)
        if not text or cut or estimate_one({"role": "user", "content": text}) > budget:
            return None
    parts = _split(text)
    if parts is None:
        t = _titles_of(rows, chain)
        return t[:MAX_TITLE], t, True
    return parts[0], parts[1], False


KEEP_TAIL = 6  # rows at the end of the view that never fold — the live tail
SPAN = 8  # rows per fold at the watermark — small enough to summarize, large enough to matter


async def write_fold(
    ctx: ProjectionCtx, rows: list[dict], idxs: list[int]
) -> dict | None:
    """Summarize `idxs` and append ONE fold row for them — or None when the summary
    would not fit its budget (nothing folded). A fold row in `idxs` is just a member —
    that is how folds nest: the next fold over this stretch of the view takes the
    earlier fold as one of its rows, and its level is one above the deepest member."""
    from ai_core.core import _save_history  # lazy: core imports this package

    ids = [rows[i].get("id") for i in idxs]
    lvl = 1 + max((level(rows, rows[i]) for i in idxs if is_fold(rows[i])), default=0)
    # THE INDEX IS INSIDE THE BUDGET: built first, its size reserved from the summary's.
    index = mechanical_index(rows, idxs)
    reserve = estimate_one({"role": "user", "content": index}) if index else 0
    summarized = await _summarize(ctx, rows, idxs, reserve)
    if summarized is None:
        return None
    title, text, degraded = summarized
    if index:
        text = f"{text}\n\n{index}"
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
    folded = refused = 0
    if estimate_tokens(view) > budget and len(rows) > 2:
        target = int(budget * LOW)
        # ONE MOVE, REPEATED: fold the oldest stretch of the view — rows, folds, or a
        # mix — until under the low watermark or nothing foldable is left.
        while estimate_tokens(view) > target:
            span = next_span(rows, view)
            if not span:
                break
            fr = await write_fold(ctx, rows, span)
            if fr is None:
                refused += 1  # the summary would not fit: this span stays; stop here
                break
            rows.append(fr)
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
    while not refused:
        span = next_span(rows, view)
        if not span or not all(is_fold(rows[i]) for i in span):
            break
        fr = await write_fold(ctx, rows, span)
        if fr is None:
            refused += 1
            break
        rows.append(fr)
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
        fold_refused=refused,
    )
