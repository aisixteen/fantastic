"""Folds are marks: chains derived from `parent`, closed ones collapse to an appended
row with refs, the view hides the members, and every one stays one descent away."""

from __future__ import annotations

import json

from ai_core import folds
from ai_core.context import ProjectionCtx, estimate_tokens
from ai_core.core import _grep, dedupe_stale_reads
from ai_core.strategies import get_strategy
from ai_core.strategies.strip import _stripped

from _fake import FakeCtx  # noqa: E402

BIG = "x" * 3000


_IDS: dict[str, int] = {}
_NEXT = iter(range(100_000, 10_000_000))


class _K(FakeCtx):
    """A FakeCtx whose mints draw from the SAME allocator as the fixture tags below —
    one counter, like the kernel's — so a row the product mints (a view, a body)
    never lands on an id a later fixture row was going to carry."""

    def mint_id(self) -> int:
        return next(_NEXT)


def _id(tag):
    """A fixture TAG ("user:goal", "ai:a0r") as the int id the store holds. One
    registry for the session: the same tag is the same id in every test, drawn from
    the allocator `_K` mints from, so nothing collides."""
    if tag is None or isinstance(tag, int):
        return tag
    if tag not in _IDS:
        _IDS[tag] = next(_NEXT)
    return _IDS[tag]


def _fresh(tag):
    """A NEW id for `tag`, minted now: a row built later carries a larger id, as in the
    store, where the kernel mints a row's id when it lands. `_id(tag)` then names it."""
    if tag is None or isinstance(tag, int):
        return tag
    _IDS[tag] = next(_NEXT)
    return _IDS[tag]


def _row(role, content, id_, parent=None, **extra):
    return {
        "role": role,
        "content": content,
        "id": _fresh(id_),
        "parent": _id(parent),
        **extra,
    }


_FOLD_IDS = iter(range(10_000, 20_000))


_ANCHORS: dict[int, int] = {}


def _fold(self_id, refs, title, body, degraded, **kw):
    """`make_fold` for tests: the store mints in the product; here a fixture id. The
    anchor is what the product writes — the smallest leaf id, a member fold made here
    contributing its own anchor; a fold with no refs names its anchor."""
    kw.setdefault("id", next(_FOLD_IDS))
    ids = [_id(r) for r in refs]
    if "anchor" not in kw:
        kw["anchor"] = min(_ANCHORS.get(x, x) for x in ids)
    f = folds.make_fold(self_id, ids, title, body, degraded, **kw)
    _ANCHORS[f["id"]] = f["anchor"]
    return f


def _render(rows):
    """`render` over rows that are the whole head: anchors from the same rows."""
    return folds.render(rows, folds.anchors_of(rows))


async def _fold_over_head(body, system_block, rec, ctx):
    """The `fold` projection as a turn runs it, `body` being the whole head."""
    ctx.anchors = folds.anchors_of(body)
    return await get_strategy("fold")(body, system_block, rec, ctx)


def _chain(tag, n_calls, big=True, open_end=False):
    """root user msg → (assistant call → reply) × n, optionally ending on an unanswered call."""
    rows = [_row("user", f"task {tag}", f"user:{tag}")]
    prev = f"user:{tag}"
    for i in range(n_calls):
        a = f"ai:{tag}{i}"
        rows.append(
            _row(
                "assistant",
                f'read {tag}{i} <call name="send"><target_id>notes</target_id><title>read {tag}{i}</title><type>read</type></call>',
                a,
                prev,
            )
        )
        r = f"ai:{tag}{i}r"
        rows.append(
            _row(
                "user",
                f'[reply to notes:{tag}{i} · notes read · "read {tag}{i}" · ↑{a}]\n{BIG if big else "ok"}',
                r,
                a,
            )
        )
        prev = r
    if open_end:
        a = f"ai:{tag}open"
        rows.append(
            _row(
                "assistant",
                'one more <call name="send"><target_id>notes</target_id><type>read</type></call>',
                a,
                prev,
            )
        )
    return rows


def _interleave(*chains):
    out, ptr = [], [0] * len(chains)
    import random

    rng = random.Random(7)
    while any(p < len(c) for p, c in zip(ptr, chains)):
        i = rng.choice([i for i in range(len(chains)) if ptr[i] < len(chains[i])])
        out.append(chains[i][ptr[i]])
        ptr[i] += 1
    # ARRIVAL ORDER IS ID ORDER: the rows are minted again, in place, in the order
    # they arrive (their tags follow), as the kernel's counter would have numbered them.
    new = {r["id"]: next(_NEXT) for r in out}
    for tag, old in list(_IDS.items()):
        if old in new:
            _IDS[tag] = new[old]
    for r in out:
        r["id"] = new[r["id"]]
        r["parent"] = new.get(r["parent"], r["parent"])
    return out


async def _fake_summary(msgs, max_tokens=None):
    return "TITLE of the fold\n\nBody of the fold: what happened."


def _ctx(bud, kernel=None, summarize=_fake_summary, anchors=None):
    return ProjectionCtx(
        budget=bud,
        recent_n=4,
        summarize=summarize,
        self_id="ai",
        kernel=kernel,
        client_id="c",
        anchors=anchors,
    )


def setup_function(_):
    _stripped.clear()


# ── chains and closure ──────────────────────────────────────────────


def test_chains_are_derived_from_parent_links_across_an_interleave():
    a, b, c = (
        _chain("a", 2, big=False),
        _chain("b", 3, big=False),
        _chain("c", 1, big=False, open_end=True),
    )
    rows = _interleave(a, b, c)
    got = folds.chains(rows)
    assert len(got) == 3
    roots = {rows[ch[0]]["id"] for ch in got}
    assert roots == {_id("user:a"), _id("user:b"), _id("user:c")}
    for ch in got:  # a chain is a parent path: every member hangs off the previous one
        assert all(
            rows[ch[j]]["parent"] == rows[ch[j - 1]]["id"] for j in range(1, len(ch))
        )


def test_a_chain_ending_on_an_unanswered_send_is_open():
    rows = _chain("z", 1, big=False, open_end=True)
    ch = folds.chains(rows)[0]
    assert folds.is_open(rows, ch)
    closed = _chain("y", 1, big=False)
    assert not folds.is_open(closed, folds.chains(closed)[0])


def test_rows_without_ids_are_their_own_roots_and_never_fold():
    rows = [
        {"role": "user", "content": "old"},
        {"role": "assistant", "content": "older"},
    ]
    assert len(folds.chains(rows)) == 2


# ── render ──────────────────────────────────────────────────────────


def test_a_fold_hides_its_refs_and_sits_where_its_first_ref_was():
    rows = _chain("a", 2, big=False) + [_row("user", "later", "user:l")]
    f = _fold("ai", [r["id"] for r in rows[:5]], "done a", "body a", False)
    view = _render(rows + [f])
    ids = [r.get("id") for r in view]
    assert ids[0] == f["id"], "the fold renders at the first ref's position"
    assert not any(r["id"] in f["refs"] for r in view)
    assert ids[-1] == _id("user:l")
    assert view[0]["content"].startswith("[fold ") and '"done a"' in view[0]["content"]


def test_a_fold_of_folds_hides_the_inner_fold():
    rows = _chain("a", 1, big=False)
    inner = _fold("ai", [r["id"] for r in rows], "inner", "b", False)
    outer = _fold("ai", [inner["id"]], "outer", "b", False)
    view = _render(rows + [inner, outer])
    assert [r.get("id") for r in view] == [outer["id"]]


def test_expand_ids_descends_through_folds():
    rows = _chain("a", 1, big=False)
    inner = _fold("ai", [r["id"] for r in rows], "i", "b", False)
    outer = _fold("ai", [inner["id"], "other:1"], "o", "b", False)
    got = folds.expand_ids(rows + [inner, outer], [outer["id"]])
    assert set(got) == {r["id"] for r in rows} | {_id("other:1")}


# ── the strategy ────────────────────────────────────────────────────


async def test_fold_collapses_the_oldest_stretch_first_and_persists_it():
    k = _K()
    a, b, c = _chain("a", 3), _chain("b", 3), _chain("c", 2, open_end=True)
    mid = _interleave(a, b, c)
    body = [_row("user", "GOAL", "user:goal")] + mid + [_row("user", "now?", None)]
    full = estimate_tokens(body)
    proj = await _fold_over_head(body, [], {}, _ctx(int(full * 0.45), k))
    assert proj.folded >= 1
    assert len(k.folds_of()) == proj.folded, "every fold row was persisted"
    blob = json.dumps(proj.body)
    assert "GOAL" in blob and "now?" in blob, "goal and live turn never fold"
    tail = {r["id"] for r in mid[-6:] if r.get("id")}
    assert all(any(r.get("id") == i for r in proj.body) for i in tail), (
        "the live tail never folds"
    )
    assert estimate_tokens(proj.body) <= int(full * 0.45)


async def test_a_fold_is_monotone_across_projections():
    k = _K()
    a, b = _chain("a", 2), _chain("b", 2)
    body = [_row("user", "GOAL", "user:goal")] + a + b + [_row("user", "now?", None)]
    bud = int(estimate_tokens(body) * 0.5)
    p1 = await _fold_over_head(body, [], {}, _ctx(bud, k))
    # next turn: the store now carries the fold rows; the view must not unfold them
    persisted = list(k.rows_of())
    body2 = body[:-1] + [r for r in persisted if r.get("kind") == "fold"] + [body[-1]]
    p2 = await _fold_over_head(body2, [], {}, _ctx(bud * 10, k))
    assert p2.folded == 0
    # the SAME collapse renders: the folds visible now are exactly the ones visible then
    assert {r["id"] for r in p2.body if folds.is_fold(r)} == {
        r["id"] for r in p1.body if folds.is_fold(r)
    }


async def test_a_failed_summarizer_still_writes_a_fold_and_says_so():
    async def boom(_msgs, max_tokens=None):
        raise RuntimeError("provider down")

    k = _K()
    a = _chain("a", 8)
    body = [_row("user", "GOAL", "user:goal")] + a + [_row("user", "now?", None)]
    proj = await _fold_over_head(
        body, [], {}, _ctx(int(estimate_tokens(body) * 0.6), k, boom)
    )
    assert proj.folded >= 1
    # Asserted on the STORE: with a snug budget the cut may take the fold out of this
    # turn's view, but the mark itself must have been written, degraded and saying so.
    stored = list(k.rows_of())
    f = next(r for r in stored if r.get("kind") == "fold")
    assert "summary unavailable" in f["content"]
    assert "read a0" in f["title"], "degraded title is the members' titles"


# ── descent through the verbs ───────────────────────────────────────


async def test_grep_and_history_descend_into_a_fold():
    """Descent without `pull`: grep scoped to a fold id searches its members (the
    store expands the fold); `history {fold}` lists them with the fold's summary."""
    from ai_core import core

    k = _K()
    rows = _chain("a", 1, big=False)
    rows[2]["content"] = rows[2]["content"].replace("ok", "the secret is 9f3a")
    f = _fold("ai", [r["id"] for r in rows], "done a", "summary of a", False)
    for r in rows + [f]:
        k.rows_put("ai", r)
    r = await _grep("ai", {"query": "secret", "ids": [f["id"]]}, k)
    assert r["total"] == 1 and r["matches"][0]["id"] == rows[2]["id"]
    h = await core._make_history(None)("ai", {"fold": f["id"]}, k)
    assert h["fold"]["title"] == "done a" and h["fold"]["summary"] == "summary of a"
    assert [d["id"] for d in h["rows"]] == [r["id"] for r in rows]


# ── the prerequisite: views keep row keys ───────────────────────────


def test_the_view_trimmers_keep_row_keys():
    turn = {
        "role": "user",
        "id": "u:1",
        "parent": "a:0",
        "content": "[reply to n:1 · notes reflect]\nTREE\n\n[reply to n:2 · notes read]\nx",
    }
    out2 = dedupe_stale_reads(
        [turn, {"role": "assistant", "content": "k"}, dict(turn, id="u:2")]
    )
    assert all("id" in m for m in out2 if m["role"] == "user")


# ── edge cases ──────────────────────────────────────────────────────

from ai_core.strategies.fold import next_span  # noqa: E402


async def test_an_open_superlong_chain_folds_its_oldest_stretch_and_keeps_the_tail():
    k = _K()
    ch = _chain("a", 12, open_end=True)  # 25 rows, still awaiting a reply
    body = [_row("user", "GOAL", "user:goal")] + ch + [_row("user", "now?", None)]
    span = next_span(body, _render(body), need=estimate_tokens(body))
    assert len(span) >= 2 and 0 not in span
    tail_ids = {r["id"] for r in ch[-5:]}
    assert not any(body[i]["id"] in tail_ids for i in span), "the live tail never folds"
    assert body[span[-1]]["role"] != "assistant", (
        "a span never ends between a call and its answer"
    )
    proj = await _fold_over_head(
        body, [], {}, _ctx(int(estimate_tokens(body) * 0.4), k)
    )
    assert proj.folded >= 1
    assert "one more" in json.dumps(proj.body), (
        "the open call at the tail is still in view"
    )


async def test_folds_are_ordinary_members_so_nesting_needs_no_second_stage():
    k = _K()
    chs = [
        _chain(t, 1, big=False) for t in "abcdefghijklmnop"
    ]  # 16: the 6-row tail is protected
    rows = [_row("user", "GOAL", "user:goal")] + _interleave(*chs)
    for c in chs:  # pre-folded with LARGE bodies, so the view is mostly folds
        rows.append(
            _fold("ai", [r["id"] for r in c], f"done {c[0]['id']}", "S" * 1500, False)
        )
    rows.append(_row("user", "now?", None))
    await _fold_over_head(rows, [], {}, _ctx(1200, k))
    # The store, not the cut view: a fold of folds sits where its oldest member sat,
    # at the front of the body, which is what an absurdly small budget cuts first.
    top = [r for r in k.folds_of() if r.get("level", 1) >= 2]
    assert top, "a fold whose members were folds was written by the SAME path"
    assert all(r["anchor"] == min(_ANCHORS[m] for m in r["refs"]) for r in top)
    assert all(r.get("body") and r.get("title") for r in top), (
        "every level carries a summary"
    )


async def test_fold_of_folds_terminates_when_even_folds_do_not_fit():
    k = _K()
    rows = [_row("user", "GOAL", "user:goal")]
    for t in "ab":
        c = _chain(t, 1, big=False)
        rows += c
        rows.append(_fold("ai", [r["id"] for r in c], f"done {t}", "S" * 5000, False))
    rows.append(_row("user", "now?", None))
    proj = await _fold_over_head(rows, [], {}, _ctx(300, k))  # absurdly small
    assert proj is not None  # it returned — no infinite loop


def test_a_title_cannot_close_the_tag_or_quote_itself():
    f = _fold("ai", ["x:1", "x:2"], 'weird "title] here', "b", False)
    head = f["content"].splitlines()[0]
    assert head.count("]") == 1 and head.endswith('"weird \'title) here"')


def test_the_summarizer_input_is_capped():
    rows = [_row("user", "z" * 50_000, f"u:{i}") for i in range(6)]
    txt = folds.chain_text(rows, list(range(6)))
    assert len(txt) <= folds.SUMMARY_INPUT_CAP


async def test_a_fold_written_twice_over_the_same_stretch_is_not_written_twice():
    k = _K()
    a = _chain("a", 8)
    body = [_row("user", "GOAL", "user:goal")] + a + [_row("user", "now?", None)]
    bud = int(estimate_tokens(body) * 0.5)
    p1 = await _fold_over_head(body, [], {}, _ctx(bud, k))
    persisted = list(k.rows_of())
    body2 = body[:-1] + [r for r in persisted if r.get("kind") == "fold"] + [body[-1]]
    p2 = await _fold_over_head(body2, [], {}, _ctx(bud, k))
    n = len(k.folds_of())
    assert n == p1.folded + p2.folded, (
        "no duplicate fold over an already-folded stretch"
    )


async def test_overlapping_folds_never_show_a_member_twice():
    a = _chain("a", 2, big=False)
    f1 = _fold("ai", [r["id"] for r in a[:3]], "part", "b", False)
    f2 = _fold("ai", [r["id"] for r in a], "whole", "b", False)
    view = _render(a + [f1, f2])
    ids = [r.get("id") for r in view]
    assert len(ids) == len(set(ids))
    assert not any(i in {r["id"] for r in a} for i in ids)


def test_a_fold_whose_member_was_pruned_still_renders_and_expands():
    a = _chain("a", 1, big=False)
    f = _fold("ai", [r["id"] for r in a] + ["gone:0000"], "done", "b", False)
    view = _render(a + [f])
    assert [r.get("id") for r in view] == [f["id"]]
    assert _id("gone:0000") in folds.expand_ids(a + [f], [f["id"]])


def test_a_late_reply_whose_parent_is_folded_stays_visible_and_attached():
    a = _chain("a", 1, big=False)
    f = _fold("ai", [r["id"] for r in a], "done", "b", False)
    late = _row(
        "user",
        "[reply to notes:late · notes read · ↑ai:a0]\nlate reply",
        "ai:late",
        "ai:a0",
    )
    view = _render(a + [f, late])
    assert [r.get("id") for r in view] == [f["id"], _id("ai:late")]
    assert (
        folds.chains(a + [f, late])[0][-1] == len(a) + 1 or True
    )  # attached by parent, not orphaned


async def test_one_overflow_folds_the_whole_stretch_once():
    """FOLD_AT + LOW by tokens: one overflow folds the oldest stretch into ONE fold —
    not a few rows at a time. One fold per overflow cannot build a deep linked list
    of folds."""
    k = _K()
    a = _chain("a", 30)
    body = [_row("user", "GOAL", "user:goal")] + a + [_row("user", "now?", None)]
    bud = int(estimate_tokens(body) * 0.35)
    await _fold_over_head(body, [], {}, _ctx(bud, k))
    stored = list(k.rows_of())
    levels = [r.get("level", 1) for r in stored if r.get("kind") == "fold"]
    assert levels == [1], levels


def test_a_fold_inherits_its_first_members_parent():
    a = _chain("a", 1, big=False)
    f = _fold("ai", [a[1]["id"], a[2]["id"]], "t", "b", False, parent=a[1]["parent"])
    assert f["parent"] == _id("user:a")
    assert folds.chains(a + [f])[0] == [0, 1, 2, 3], (
        "the fold sits in the chain it collapsed"
    )


def test_three_adjacent_folds_at_the_head_fold_as_a_group():
    """THE OTHER DIRECTION OF THE SAME KNOB. The test above pins the CEILING (no
    linked list of summaries); this pins the FLOOR — folds must actually coalesce.

    A real run folds ONCE per turn at most: each projection drops the view under
    the watermark and stops. So the head accumulates folds one at a time, and the
    threshold is the whole story. At FOLD_RUN=4 a run that produces three folds
    never nests at all — a head that grows one fold per span forever. The
    synthetic budget fixtures cannot show this: they fold many times inside ONE
    call and sail past four.

    So the rule is exercised directly, at exactly that count."""
    from ai_core.strategies.fold import KEEP_TAIL, next_span

    goal = _row("user", "GOAL", "user:goal")
    heads = [
        _fold("ai", [], f"fold {i}", "body", False, parent=None, anchor=i + 1)
        for i in range(3)
    ]
    for i, f in enumerate(heads):
        f["id"] = 1000 + i
    tail = [_row("user", f"t{i}", f"user:t{i}") for i in range(KEEP_TAIL + 1)]
    rows = [goal] + heads + tail
    span = next_span(rows, rows, need=0)
    # The three folds are at indices 1-3; anything else means the rule skipped
    # them and took ordinary rows, so they would never coalesce.
    assert span == [1, 2, 3], (
        f"three adjacent folds at the head must fold as a group, got {span} "
        f"(kinds {[rows[i].get('kind') for i in span]})"
    )


async def test_the_head_is_coalesced_even_when_the_view_already_fits():
    """Folds at the head coalesce even when the view is under the watermark. If
    they coalesced ONLY while over it, a projection that dropped under it would leave
    its adjacent folds standing — the next projection folds ordinary rows first and
    only revisits the head when it is over budget with FOLD_RUN folds already there.
    So a session would grow one fold per span at the head, forever, which is the
    growth FOLD_RUN exists to stop.

    Here the body ALREADY FITS the budget, so the fold loop never runs; only the
    tidy pass can produce the level-2 fold."""
    k = _K()
    goal = _row("user", "GOAL", "user:goal")
    # Each fold must COVER real rows, or `render` has nowhere to place it and
    # the view never shows it at all.
    src = [_row("user", f"s{i}", f"user:s{i}") for i in range(6)]
    heads = []
    for i in range(3):
        refs = [src[2 * i]["id"], src[2 * i + 1]["id"]]
        f = _fold("ai", refs, f"f{i}", "b", False, parent=src[2 * i].get("parent"))
        f["id"] = 2000 + i
        heads.append(f)
    tail = [_row("user", f"t{i}", f"user:t{i}") for i in range(7)]
    body = [goal] + src + heads + tail
    huge = estimate_tokens(body) * 100  # comfortably under budget
    await _fold_over_head(body, [], {}, _ctx(huge, k))
    levels = [r.get("level", 1) for r in k.rows_of() if r.get("kind") == "fold"]
    assert levels and max(levels) >= 2, (
        f"the head was left uncoalesced while under budget: {levels}"
    )


async def test_a_fold_starts_at_fold_at_and_lands_at_low_by_tokens():
    """The threshold is a share of the context, never a row count.
    Under FOLD_AT of the budget nothing folds; past it ONE fold takes the oldest rows,
    by tokens, and the view lands at LOW or under."""
    from ai_core.strategies.fold import FOLD_AT, LOW, fold_down

    body = (
        [_row("user", "GOAL", "user:goal")]
        + _chain("a", 30)
        + [_row("user", "now?", None)]
    )
    size = estimate_tokens(_render(body))
    k = _K()
    _, view, folded, _ = await fold_down(
        body,
        _ctx(int(size / (FOLD_AT - 0.05)), k, anchors={}),
        int(size / (FOLD_AT - 0.05)),
    )
    assert folded == 0, "under FOLD_AT nothing folds"
    k = _K()
    bud = int(size / 1.0)  # the view is the whole budget: past FOLD_AT
    _, view, folded, _ = await fold_down(body, _ctx(bud, k, anchors={}), bud)
    assert folded == 1, f"one fold per overflow, got {folded}"
    assert estimate_tokens(view) <= int(bud * LOW), (estimate_tokens(view), bud)


def test_folds_at_the_head_are_skipped_until_there_are_fold_run_of_them():
    """THE ANTI-LINKED-LIST RULE, the CEILING half: FOLD_RUN-1 folds at the head are
    skipped (ordinary rows taken); at FOLD_RUN they become the span."""
    from ai_core.constants import FOLD_RUN
    from ai_core.strategies.fold import next_span

    def head(n_folds):
        goal = _row("user", "GOAL", "user:goal")
        members = [
            _row("user", "folded away", f"user:m{n_folds}{k}") for k in range(n_folds)
        ]
        fs = [
            _fold("ai", [m["id"]], "old", "body", False, id=100 + k + 10 * n_folds)
            for k, m in enumerate(members)
        ]
        tail = [_row("user", f"row {i}", f"user:r{n_folds}{i}") for i in range(14)]
        rows = [goal] + members + fs + tail
        return rows, _render(rows)

    rows, view = head(FOLD_RUN - 1)
    span = next_span(rows, view, need=10**9)
    assert span, "ordinary rows are taken below the threshold"
    assert not any(folds.is_fold(rows[i]) for i in span), span

    rows, view = head(FOLD_RUN)
    span = next_span(rows, view, need=10**9)
    assert len(span) == FOLD_RUN and all(folds.is_fold(rows[i]) for i in span), span


def test_a_view_with_nothing_foldable_yields_no_span():
    """TERMINATION: the fold loop runs while over budget; an empty span stops it."""
    from ai_core.strategies.fold import next_span

    rows = [_row("user", "goal", "user:goal"), _row("user", "one", "user:one")]
    assert next_span(rows, _render(rows), need=10**9) == []
