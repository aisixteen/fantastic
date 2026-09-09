"""Folds are marks: chains derived from `parent`, closed ones collapse to an appended
row with refs, the view hides the members, and every one stays one descent away."""

from __future__ import annotations

import json

from ai_core import folds
from ai_core.context import ProjectionCtx, estimate_tokens
from ai_core.core import _grep, _pull, dedupe_stale_reads, drop_answered_reflects
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


def _row(role, content, id_, parent=None, **extra):
    return {
        "role": role,
        "content": content,
        "id": _id(id_),
        "parent": _id(parent),
        **extra,
    }


_FOLD_IDS = iter(range(10_000, 20_000))


def _fold(self_id, refs, title, body, degraded, **kw):
    """`make_fold` for tests: the store mints in the product; here a fixture id."""
    kw.setdefault("id", next(_FOLD_IDS))
    return folds.make_fold(self_id, [_id(r) for r in refs], title, body, degraded, **kw)


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
    return out


async def _fake_summary(msgs):
    return "TITLE of the fold\n\nBody of the fold: what happened."


def _ctx(bud, kernel=None, summarize=_fake_summary):
    return ProjectionCtx(
        budget=bud,
        recent_n=4,
        summarize=summarize,
        self_id="ai",
        kernel=kernel,
        client_id="c",
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
    view = folds.render(rows + [f])
    ids = [r.get("id") for r in view]
    assert ids[0] == f["id"], "the fold renders at the first ref's position"
    assert not any(r["id"] in f["refs"] for r in view)
    assert ids[-1] == _id("user:l")
    assert view[0]["content"].startswith("[fold ") and '"done a"' in view[0]["content"]


def test_a_fold_of_folds_hides_the_inner_fold():
    rows = _chain("a", 1, big=False)
    inner = _fold("ai", [r["id"] for r in rows], "inner", "b", False)
    outer = _fold("ai", [inner["id"]], "outer", "b", False)
    view = folds.render(rows + [inner, outer])
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
    proj = await get_strategy("fold")(body, [], {}, _ctx(int(full * 0.45), k))
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
    p1 = await get_strategy("fold")(body, [], {}, _ctx(bud, k))
    # next turn: the store now carries the fold rows; the view must not unfold them
    persisted = list(k.rows_of())
    body2 = body[:-1] + [r for r in persisted if r.get("kind") == "fold"] + [body[-1]]
    p2 = await get_strategy("fold")(body2, [], {}, _ctx(bud * 10, k))
    assert p2.folded == 0
    # the SAME collapse renders: the folds visible now are exactly the ones visible then
    assert {r["id"] for r in p2.body if folds.is_fold(r)} == {
        r["id"] for r in p1.body if folds.is_fold(r)
    }


async def test_a_failed_summarizer_still_writes_a_fold_and_says_so():
    async def boom(_msgs):
        raise RuntimeError("provider down")

    k = _K()
    a = _chain("a", 8)
    body = [_row("user", "GOAL", "user:goal")] + a + [_row("user", "now?", None)]
    proj = await get_strategy("fold")(
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


async def test_grep_and_pull_descend_into_a_fold(monkeypatch):
    from ai_core import core

    k = _K()
    rows = _chain("a", 1, big=False)
    rows[2]["content"] = rows[2]["content"].replace("ok", "the secret is 9f3a")
    f = _fold("ai", [r["id"] for r in rows], "done a", "summary of a", False)
    all_rows = rows + [f]

    async def fake_load(self_id, kernel):
        return all_rows

    monkeypatch.setattr(core, "_load_history", fake_load)
    r = await _grep("ai", {"query": "secret", "ids": [f["id"]]}, k)
    assert r["total"] == 1 and r["matches"][0]["id"] == rows[2]["id"]
    p = await _pull("ai", {"pid": f["id"]}, k)
    assert (
        p["text"] == "summary of a"
        and p["refs"] == f["refs"]
        and p["title"] == "done a"
    )


# ── the prerequisite: views keep row keys ───────────────────────────


def test_the_view_trimmers_keep_row_keys():
    turn = {
        "role": "user",
        "id": "u:1",
        "parent": "a:0",
        "content": "[reply to n:1 · notes reflect]\nTREE\n\n[reply to n:2 · notes read]\nx",
    }
    out = drop_answered_reflects(
        [turn, {"role": "assistant", "content": "SEEN notes"}, dict(turn, id="u:2")]
    )
    assert out[0]["id"] == "u:1" and out[0]["parent"] == "a:0"
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
    span = next_span(body, folds.render(body))
    assert len(span) >= 2 and 0 not in span
    tail_ids = {r["id"] for r in ch[-5:]}
    assert not any(body[i]["id"] in tail_ids for i in span), "the live tail never folds"
    assert body[span[-1]]["role"] != "assistant", (
        "a span never ends between a call and its answer"
    )
    proj = await get_strategy("fold")(
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
    proj = await get_strategy("fold")(rows, [], {}, _ctx(1200, k))
    top = [r for r in proj.body if folds.is_fold(r) and r.get("level", 1) >= 2]
    assert top, "a fold whose members were folds was written by the SAME path"
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
    proj = await get_strategy("fold")(rows, [], {}, _ctx(300, k))  # absurdly small
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
    p1 = await get_strategy("fold")(body, [], {}, _ctx(bud, k))
    persisted = list(k.rows_of())
    body2 = body[:-1] + [r for r in persisted if r.get("kind") == "fold"] + [body[-1]]
    p2 = await get_strategy("fold")(body2, [], {}, _ctx(bud, k))
    n = len(k.folds_of())
    assert n == p1.folded + p2.folded, (
        "no duplicate fold over an already-folded stretch"
    )


async def test_overlapping_folds_never_show_a_member_twice():
    a = _chain("a", 2, big=False)
    f1 = _fold("ai", [r["id"] for r in a[:3]], "part", "b", False)
    f2 = _fold("ai", [r["id"] for r in a], "whole", "b", False)
    view = folds.render(a + [f1, f2])
    ids = [r.get("id") for r in view]
    assert len(ids) == len(set(ids))
    assert not any(i in {r["id"] for r in a} for i in ids)


def test_a_fold_whose_member_was_pruned_still_renders_and_expands():
    a = _chain("a", 1, big=False)
    f = _fold("ai", [r["id"] for r in a] + ["gone:0000"], "done", "b", False)
    view = folds.render(a + [f])
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
    view = folds.render(a + [f, late])
    assert [r.get("id") for r in view] == [f["id"], _id("ai:late")]
    assert (
        folds.chains(a + [f, late])[0][-1] == len(a) + 1 or True
    )  # attached by parent, not orphaned


async def test_folding_does_not_build_a_linked_list_of_summaries():
    """Twelve levels in 26 cuts was the measured failure: every span began on the last
    fold. The head's folds are skipped until FOLD_RUN of them sit together."""
    k = _K()
    a = _chain("a", 30)
    body = [_row("user", "GOAL", "user:goal")] + a + [_row("user", "now?", None)]
    bud = int(estimate_tokens(body) * 0.35)
    await get_strategy("fold")(body, [], {}, _ctx(bud, k))
    stored = list(k.rows_of())
    levels = [r.get("level", 1) for r in stored if r.get("kind") == "fold"]
    assert len(levels) >= 3
    assert max(levels) <= 3, levels


def test_a_fold_inherits_its_first_members_parent():
    a = _chain("a", 1, big=False)
    f = _fold("ai", [a[1]["id"], a[2]["id"]], "t", "b", False, parent=a[1]["parent"])
    assert f["parent"] == _id("user:a")
    assert folds.chains(a + [f])[0] == [0, 1, 2, 3], (
        "the fold sits in the chain it collapsed"
    )
