"""A fold carries its own place (`anchor`): it stays in the view when its members are
not in the rows being rendered (a body rebuilt from the view array, a store compacted
by `context_forget`), and a store written before the field existed renders as it did."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

from _testkit import as_agent, memory_meta, wire

from ai_core import core, folds
from ai_core.strategies.fold import write_fold

from test_folds import (  # noqa: E402
    _K,
    _chain,
    _ctx,
    _fold,
    _fold_over_head,
    _interleave,
    _row,
)

DONE = '<call name="done"></call>'
CALL = '<call name="send"><target_id>notes</target_id><type>read</type></call>'
DUMP = Path(__file__).resolve().parents[7] / "contract/ai_core/dump_before_anchors.json"


def _old_render(rows: list[dict]) -> list[dict]:
    """`render` as it was before anchors: each fold at the position of the first of its
    refs present in `rows`. The oracle the new render must match while every member
    is present."""
    cov = folds.covered(rows)
    first_pos: dict = {}
    for i, r in enumerate(rows):
        rid = r.get("id")
        if rid in cov and rid not in first_pos:
            first_pos[rid] = i
    place: dict[int, list[dict]] = {}
    for r in rows:
        if folds.is_fold(r):
            refs = r.get("refs") or []
            at = min((first_pos[x] for x in refs if x in first_pos), default=None)
            if at is not None and r.get("id") not in cov:
                place.setdefault(at, []).append(r)
    out: list[dict] = []
    for i, r in enumerate(rows):
        for f in place.get(i, ()):
            out.append(f)
        if r.get("id") in cov or folds.is_fold(r):
            continue
        out.append(r)
    return out


def _new_render(rows: list[dict]) -> list[dict]:
    return folds.render(rows, folds.anchors_of(rows))


def _ids(rows: list[dict]) -> list:
    return [r.get("id") for r in rows]


def _bytes(rows: list[dict]) -> str:
    return json.dumps(rows, sort_keys=True, ensure_ascii=False, default=str)


def _cascade(rows: list[dict], gone: set) -> list[dict]:
    """What the store's `prune` does: the rows go, and each leaves every fold's refs."""
    out = []
    for r in rows:
        if r.get("id") in gone:
            continue
        if folds.is_fold(r):
            r = {**r, "refs": [m for m in r.get("refs") or () if m not in gone]}
        out.append(r)
    return out


# ── the fixtures of the fold tests, members present ─────────────────


def _fixtures() -> dict[str, list[dict]]:
    out: dict[str, list[dict]] = {}
    rows = _chain("a", 2, big=False) + [_row("user", "later", "user:l")]
    out["one fold"] = rows + [_fold("ai", [r["id"] for r in rows[:5]], "a", "b", False)]
    rows = _chain("a", 1, big=False)
    inner = _fold("ai", [r["id"] for r in rows], "inner", "b", False)
    out["fold of folds"] = rows + [
        inner,
        _fold("ai", [inner["id"]], "outer", "b", False),
    ]
    a = _chain("a", 2, big=False)
    out["overlapping"] = a + [
        _fold("ai", [r["id"] for r in a[:3]], "part", "b", False),
        _fold("ai", [r["id"] for r in a], "whole", "b", False),
    ]
    a = _chain("a", 1, big=False)
    out["a member gone"] = a + [
        _fold("ai", [r["id"] for r in a] + ["gone:0000"], "done", "b", False)
    ]
    a = _chain("a", 1, big=False)
    f = _fold("ai", [r["id"] for r in a], "done", "b", False)
    late = _row("user", "[reply to notes:late]\nlate", "ai:late", "ai:a0")
    out["a late reply"] = a + [f, late]
    chs = [_chain(t, 1, big=False) for t in "abcdef"]
    rows = [_row("user", "GOAL", "user:goal")] + _interleave(*chs)
    rows += [_fold("ai", [r["id"] for r in c], "d", "S", False) for c in chs]
    out["interleaved chains"] = rows + [_row("user", "now?", "user:now")]
    a, b = _chain("a", 1, big=False), _chain("b", 1, big=False)
    body = [_row("user", "GOAL", "user:goal")] + _interleave(a, b)
    fa = _fold("ai", [r["id"] for r in a], "a", "b", False)
    fb = _fold("ai", [r["id"] for r in b], "b", "b", False)
    tail = [_row("user", f"t{i}", f"user:t{i}") for i in range(3)]
    out["two folds and a tail"] = body + [fa, fb] + tail
    return out


def test_render_is_unchanged_when_members_are_present():
    for name, rows in _fixtures().items():
        assert _bytes(_new_render(rows)) == _bytes(_old_render(rows)), name


def test_render_is_unchanged_for_a_store_written_before_anchors():
    """The committed dump, made on the code before anchors: every member present."""
    rows = [t["row"] for t in json.loads(DUMP.read_text())["tables"]["rows"]]
    refs = {
        f["fold"]: f["members"]
        for f in json.loads(DUMP.read_text())["tables"]["fold_refs"]
    }
    rows = [{**r, "refs": refs[r["id"]]} if r["id"] in refs else r for r in rows]
    last = max(i for i, r in enumerate(rows) if r.get("kind") == "clear")
    head = [r for r in rows[last:] if r.get("kind") not in ("view", "anchors")]
    assert _bytes(_new_render(head)) == _bytes(_old_render(head))


async def test_render_is_unchanged_over_folds_the_strategy_wrote():
    """A run as the product lives it: chains arrive, each projection folds what is over
    the budget, and every fold lands in the store after its members and before what
    arrives next — folds of folds included."""
    k = _K()
    rows = [_row("user", "GOAL", "user:goal")]
    for t in "abcdefghijkl":
        rows += _chain(t, 2)
        await _fold_over_head(rows, [], {}, _ctx(4000, k))
        mine = {r["id"] for r in rows}
        rows += [r for r in k.rows_of() if r["id"] not in mine]
    assert any(r.get("level", 1) >= 2 for r in rows if folds.is_fold(r))
    assert _bytes(_new_render(rows)) == _bytes(_old_render(rows))


# ── a new fold carries its anchor ───────────────────────────────────


async def test_new_folds_carry_an_anchor():
    k = _K()
    a, b, c = (_chain(t, 1, big=False) for t in "abc")
    rows = [_row("user", "GOAL", "user:goal")] + _interleave(a, b, c)
    anchors: dict = {}
    l1 = []
    for ch in (a, b, c):
        idxs = [rows.index(r) for r in ch]
        f = await write_fold(_ctx(10**6, k), rows, idxs, anchors)
        assert f["anchor"] == min(r["id"] for r in ch), (
            "an L1's anchor is its first leaf"
        )
        anchors[f["id"]] = f["anchor"]
        rows.append(f)
        l1.append(f)
    idxs = [rows.index(f) for f in l1]
    l2 = await write_fold(_ctx(10**6, k), rows, idxs, anchors)
    assert l2["level"] == 2
    assert l2["anchor"] == min(f["anchor"] for f in l1), "an L2's is its members' least"
    stored = {r["id"]: r for r in k.rows_of()}
    assert all(stored[f["id"]]["anchor"] == f["anchor"] for f in l1 + [l2])


# ── a fold without its members ──────────────────────────────────────


def test_a_fold_without_members_keeps_its_place():
    goal = _row("user", "GOAL", "user:goal")
    a, b = _chain("a", 1, big=False), _chain("b", 1, big=False)
    ab = _interleave(a, b)
    c = _chain("c", 1, big=False)
    fa = _fold("ai", [r["id"] for r in a], "a", "b", False)
    fb = _fold("ai", [r["id"] for r in b], "b", "b", False)
    fc = _fold("ai", [r["id"] for r in c], "c", "b", False)
    l2 = _fold("ai", [fa["id"], fb["id"]], "a and b", "b", False)
    tail = [_row("user", f"t{i}", f"user:t{i}") for i in range(2)]
    rows = [goal] + ab + c + [fa, fb, fc, l2] + tail
    before = _ids(_new_render(rows))
    assert before == [goal["id"], l2["id"], fc["id"]] + _ids(tail)
    # the leaves of a and b go: the L2 keeps its place, ahead of c's fold
    gone = {r["id"] for r in a + b}
    after = _cascade(rows, gone)
    assert _ids(_new_render(after)) == before
    # c's leaves go too, all but its last: c's fold stays where its first leaf was
    after = _cascade(after, {r["id"] for r in c[:-1]})
    assert _ids(_new_render(after)) == before
    # a fold whose first member is gone, others left: it does not move to a survivor
    x = _row("user", "x", "user:x")
    y = _row("assistant", "y", "user:y")
    z = _row("user", "z", "user:z")
    fx = _fold("ai", [x["id"], z["id"]], "x and z", "b", False)
    rows = [x, y, z, fx]
    assert _ids(_new_render(rows)) == [fx["id"], y["id"]]
    assert _ids(_new_render(_cascade(rows, {x["id"]}))) == [fx["id"], y["id"]]


def test_an_unanchored_fold_without_members_is_counted_not_placed():
    a = _chain("a", 1, big=False)
    f = _fold("ai", [r["id"] for r in a], "done", "b", False)
    old = {k: v for k, v in f.items() if k != "anchor"}  # written before the field
    tail = [_row("user", "now", "user:now")]
    rows = _cascade(a + [old] + tail, {r["id"] for r in a})
    anchors = folds.anchors_of(rows)
    assert old["id"] not in anchors
    assert _ids(folds.render(rows, anchors)) == _ids(tail)
    assert folds.unanchored(rows, anchors) == 1
    # an `anchors` row recorded for it places it again
    rec = {"kind": "anchors", "v": 1, "anchors": {str(old["id"]): a[0]["id"]}}
    anchors = folds.anchors_of(rows + [rec])
    assert _ids(folds.render(rows, anchors)) == [old["id"]] + _ids(tail)


async def test_the_projection_counts_unanchored_folds():
    k = _K()
    a = _chain("a", 1, big=False)
    f = _fold("ai", [r["id"] for r in a], "done", "b", False)
    old = {k_: v for k_, v in f.items() if k_ not in ("anchor", "refs")}
    body = [_row("user", "GOAL", "user:goal"), old, _row("user", "now", "user:now")]
    proj = await _fold_over_head(body, [], {}, _ctx(10**6, k))
    assert proj.unanchored == 1
    assert old["id"] not in _ids(proj.body)


# ── through a real agent and its ram_store ──────────────────────────


class _Model:
    """Answers a turn with `done`, a fold summary with a title and a body, and holds
    a turn open while `hold` is set."""

    def __init__(self):
        self.requests: list[list[dict]] = []
        self.hold: asyncio.Event | None = None
        self.entered = asyncio.Event()

    async def chat(self, messages, max_tokens=None):
        if "Summarize the conversation" in str(messages[0].get("content")):
            yield "the work, folded\n\nwhat was read and how it ended."
            return
        self.requests.append(list(messages))
        self.entered.set()
        if self.hold is not None:
            await self.hold.wait()
        yield DONE


def _agent(k, aid: str, *, window: int = 32768, store: str = "memory") -> None:
    k.create(
        "ollama_backend.tools",
        id=aid,
        endpoint="http://127.0.0.1:9/stub",
        model="stub",
        context_window=window,
        output_reserve=0,
        context_strategy="fold",
        system_prompt="You are a test agent.",
        **memory_meta(k, aid, {"cli": "*"}, store=store),
    )


def _closed(mint, tag: str, big: int = 3000) -> list[dict]:
    u = {"role": "user", "id": mint(), "parent": None, "content": f"task {tag}"}
    a = {
        "role": "assistant",
        "id": mint(),
        "parent": u["id"],
        "content": f"{tag} {CALL}",
    }
    r = {
        "role": "user",
        "id": mint(),
        "parent": a["id"],
        "content": f'[reply to notes · read · "{tag}"]\n'
        + " ".join(f"{tag}{i}" for i in range(big // 4)),
    }
    d = {"role": "assistant", "id": mint(), "parent": r["id"], "content": f"ok {DONE}"}
    return [u, a, r, d]


async def _seed(k, aid: str, rows: list[dict]) -> None:
    with as_agent(k, aid):
        await core._save_history(aid, k, rows)


async def _turn(k, aid: str, text: str = "go") -> dict:
    out = await k.send([{"target": aid, "payload": {"type": "send", "text": text}}])
    assert out[0]["status"] == "completed", out
    return out[0]["result"]


async def _call(k, aid: str, payload: dict) -> dict:
    out = await k.send([{"target": aid, "payload": payload}])
    assert out[0]["status"] == "completed", out
    return out[0]["result"]


async def _store(k, aid: str, store: str = "memory") -> list[dict]:
    with as_agent(k, aid):
        res = (await k.send([{"target": store, "payload": {"type": "read"}}]))[0]
    return res["result"]["rows"]


async def _prompt(k, aid: str) -> str:
    """The next prompt as the model reads it."""
    with as_agent(k, aid):
        msgs, _ = await core._assemble(aid, "next", k, "c")
    return _bytes(core._render_for_model(msgs))


async def _views(k, aid: str) -> list[dict]:
    return [r for r in await _store(k, aid) if r.get("kind") == "view"]


def _reaches(rows: list[dict], ids: list) -> set:
    """Every fold named in `ids`, and every fold under one of them."""
    by_id = {r["id"]: r for r in rows}
    out, stack = set(), list(ids)
    while stack:
        x = stack.pop()
        r = by_id.get(x)
        if r is None or not folds.is_fold(r) or x in out:
            continue
        out.add(x)
        stack.extend(r.get("refs") or ())
    return out


async def test_folds_survive_a_projection_over_the_view_array(
    seeded_kernel, fake_providers
):
    """The bug: a turn rebuilt from the view array holds folds without their members,
    and the next projection past TRIGGER rendered them nowhere."""
    k = seeded_kernel
    _agent(k, "fv", window=4000)
    fake_providers["fv"] = _Model()
    mint = k.ctx.mint_id
    goal = {"role": "user", "id": mint(), "parent": None, "content": "GOAL"}
    await _seed(k, "fv", [goal] + [r for t in "abcdef" for r in _closed(mint, t, 2000)])
    await _turn(k, "fv")
    rows = await _store(k, "fv")
    first = [r["id"] for r in rows if folds.is_fold(r)]
    assert first, "the first turn folded"
    assert (await _views(k, "fv"))[-1]["why"] == "projection: fold"
    # the next turn's prompt is rebuilt from that view array, and is pushed past
    # TRIGGER by what lands after it
    await _seed(k, "fv", [r for t in "ghij" for r in _closed(mint, t, 2000)])
    await _turn(k, "fv")
    assert core._view_source["fv"] == "array"
    rows = await _store(k, "fv")
    assert len([r for r in rows if folds.is_fold(r)]) > len(first), "it folded again"
    shown = [e["id"] for e in (await _views(k, "fv"))[-1]["ids"]]
    missing = [f for f in first if f not in _reaches(rows, shown)]
    assert not missing, f"folds {missing} fell out of the view {shown}"
    assert all(f in shown for f in first), (first, shown)


async def test_context_forget_prunes_covered_rows_and_old_views(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    _agent(k, "ff", window=4000)
    fake_providers["ff"] = _Model()
    mint = k.ctx.mint_id
    goal = {"role": "user", "id": mint(), "parent": None, "content": "GOAL"}
    await _seed(k, "ff", [goal] + [r for t in "abcdef" for r in _closed(mint, t, 2000)])
    await _turn(k, "ff")
    await _seed(k, "ff", [r for t in "ghij" for r in _closed(mint, t, 2000)])
    await _turn(k, "ff", "and again")
    rows = await _store(k, "ff")
    fold_rows = [r for r in rows if folds.is_fold(r)]
    assert fold_rows and all(isinstance(f.get("anchor"), int) for f in fold_rows)
    covered = set(folds.expand_ids(rows, [f["id"] for f in fold_rows]))
    views = [r["id"] for r in rows if r.get("kind") == "view"]
    assert len(views) >= 2
    before = await _prompt(k, "ff")
    out = await _call(k, "ff", {"type": "context_forget"})
    assert out == {
        "pruned": len(covered) + len(views) - 1,
        "kept_folds": len(fold_rows),
        "unanchored": 0,
    }, out
    after_rows = await _store(k, "ff")
    left = {r["id"] for r in after_rows}
    assert not covered & left, "every row a fold summarises is gone"
    assert {f["id"] for f in fold_rows} <= left, "every fold is kept"
    assert [r["id"] for r in after_rows if r.get("kind") == "view"] == views[-1:]
    assert not any(r.get("kind") == "anchors" for r in after_rows), (
        "folds that carry their anchor need no anchors row"
    )
    # the head cache is the store, no restart
    head = core._mem(k)["heads"]["ff"]
    strip = lambda rs: [{k_: v for k_, v in r.items() if k_ != "agent"} for r in rs]  # noqa: E731
    assert strip(head) == strip(after_rows)
    assert await _prompt(k, "ff") == before, "the next prompt is the same bytes"
    # a second forget has nothing left to do
    again = await _call(k, "ff", {"type": "context_forget"})
    assert again["pruned"] == 0, again
    # a pruned id is gone for recall and history; the fold still says what it was
    gone = sorted(covered)[0]
    hist = core._make_history(core.Backend(sentence="s", make_provider=None, name="t"))
    with as_agent(k, "ff"):
        h = await hist("ff", {"fold": fold_rows[0]["id"]}, k)
    assert h["fold"]["title"] == fold_rows[0]["title"] and h["fold"]["summary"]
    assert gone not in {r["id"] for r in h["rows"]}


async def test_context_forget_keeps_open_chains_and_unanchored_folds(
    seeded_kernel, fake_providers
):
    k = seeded_kernel
    _agent(k, "fo")
    fake_providers["fo"] = _Model()
    mint = k.ctx.mint_id
    goal = {"role": "user", "id": mint(), "parent": None, "content": "GOAL"}
    done = _closed(mint, "done", big=10)
    # an open chain: its last row is a send still owed a reply; its head is folded
    o = _closed(mint, "open", big=10)[:3]
    o.append(
        {"role": "assistant", "id": mint(), "parent": o[-1]["id"], "content": CALL}
    )
    # a fold written before the field, with its members: anchored by deriving
    legacy_m = _closed(mint, "legacy", big=10)
    await _seed(k, "fo", [goal] + done + o + legacy_m)
    f_done = folds.make_fold(
        "fo", [r["id"] for r in done], "d", "b", False, id=mint(), anchor=done[0]["id"]
    )
    f_open = folds.make_fold(
        "fo", [r["id"] for r in o[:2]], "o", "b", False, id=mint(), anchor=o[0]["id"]
    )
    legacy = folds.make_fold(
        "fo", [r["id"] for r in legacy_m], "l", "b", False, id=mint(), anchor=0
    )
    legacy.pop("anchor")
    # and one with neither field nor members: its place cannot be known
    lost = folds.make_fold("fo", [], "lost", "b", False, id=mint(), anchor=0)
    lost.pop("anchor")
    await _seed(k, "fo", [f_done, f_open, legacy, lost])
    out = await _call(k, "fo", {"type": "context_forget"})
    assert out == {
        "pruned": len(done) + len(legacy_m),
        "kept_folds": 4,
        "unanchored": 1,
    }, out
    left = {r["id"]: r for r in await _store(k, "fo")}
    assert all(r["id"] in left for r in o), "an open chain keeps every row"
    assert {f_done["id"], f_open["id"], legacy["id"], lost["id"]} <= set(left)
    recorded = [r for r in left.values() if r.get("kind") == "anchors"]
    assert [r["anchors"] for r in recorded] == [
        {str(legacy["id"]): legacy_m[0]["id"]}
    ], "the legacy fold's place is recorded before its members go"


def _inject_payload(dump: dict) -> dict:
    return {"type": "inject", "tables": dump["tables"]}


async def test_a_dump_from_before_anchors_restores_with_the_same_view(
    seeded_kernel, fake_providers
):
    dump = json.loads(DUMP.read_text())
    k = seeded_kernel
    k.ctx.update(k.id, guid=dump["guid"])
    aid = dump["agent"]
    top = max(t["row"]["id"] for t in dump["tables"]["rows"])
    while k.ctx.mint_id() <= top:  # the kernel's counter is past every stored id
        pass
    _agent(k, aid)
    fake_providers[aid] = _Model()
    res = await k.send([{"target": "memory", "payload": _inject_payload(dump)}])
    assert res[0]["status"] == "completed", res

    async def rendered() -> list:
        with as_agent(k, aid):
            head = await core._head(aid, k)
            hist = await core._load_history(aid, k)
        return _ids(folds.render(hist, folds.anchors_of(head)))

    assert await rendered() == dump["render_ids"], "the view main rendered"
    with as_agent(k, aid):
        msgs, _ = await core._assemble(aid, "next", k, "c")
    assert [m.get("id") for m in msgs[1:-1]] == dump["prompt_ids"]
    before = await _prompt(k, aid)
    rows = await _store(k, aid)
    last_clear = max(r["id"] for r in rows if r.get("kind") == "clear")
    head_folds = [r for r in rows if folds.is_fold(r) and r["id"] > last_clear]
    assert head_folds and not any("anchor" in f for f in head_folds)
    members = set(folds.expand_ids(rows, [f["id"] for f in head_folds]))
    out = await _call(k, aid, {"type": "context_forget"})
    assert out["unanchored"] == 0 and out["kept_folds"] == len(head_folds), out
    assert await _prompt(k, aid) == before, "the next prompt is the same bytes"
    left = await _store(k, aid)
    assert not members & {r["id"] for r in left}, "the old folds' members are gone"
    recorded = [r for r in left if r.get("kind") == "anchors"]
    assert len(recorded) == 1
    assert recorded[0]["anchors"] == {
        str(f["id"]): folds.anchors_of(rows)[f["id"]] for f in head_folds
    }
    assert await rendered() == dump["render_ids"]
    # dump → inject into a fresh store under the same guid: still the same view
    tables = (await k.send([{"target": "memory", "payload": {"type": "dump"}}]))[0]
    k.create("ram_store.tools", id="memory2")
    wire(k, aid, "memory2")
    k.ctx.update(aid, memory_store_id="memory2")
    res = await k.send(
        [
            {
                "target": "memory2",
                "payload": {"type": "inject", "tables": tables["result"]["tables"]},
            }
        ]
    )
    assert res[0]["status"] == "completed", res
    core._mem(k)["heads"].pop(aid, None)
    assert await rendered() == dump["render_ids"]
    assert await _prompt(k, aid) == before


async def test_context_forget_include_cleared(seeded_kernel, fake_providers):
    k = seeded_kernel
    _agent(k, "fc")
    fake_providers["fc"] = _Model()
    mint = k.ctx.mint_id
    old = _closed(mint, "old", big=10)
    await _seed(k, "fc", old)
    cleared = await _call(k, "fc", {"type": "context_clear", "why": "next"})
    new = _closed(mint, "new", big=10)
    await _seed(k, "fc", new)
    kept = await _call(k, "fc", {"type": "context_forget"})
    left = {r["id"] for r in await _store(k, "fc")}
    assert {r["id"] for r in old} <= left, "without include_cleared nothing behind"
    out = await _call(k, "fc", {"type": "context_forget", "include_cleared": True})
    rows = await _store(k, "fc")
    left = {r["id"] for r in rows}
    assert not {r["id"] for r in old} & left, "the rows before the clear go"
    assert cleared["cleared"] in left, "the clear row stays"
    assert {r["id"] for r in new} <= left
    assert out["pruned"] == len(old) and kept["pruned"] == 0, (kept, out)
    bad = (
        await k.send(
            [
                {
                    "target": "fc",
                    "payload": {"type": "context_forget", "include_cleared": "yes"},
                }
            ]
        )
    )[0]["result"]
    assert bad["reason"] == "bad_arg" and bad["arg"] == "include_cleared", bad


async def test_context_forget_waits_for_a_running_turn(seeded_kernel, fake_providers):
    k = seeded_kernel
    _agent(k, "fw")
    model = fake_providers["fw"] = _Model()
    model.hold = asyncio.Event()
    turn = asyncio.ensure_future(_turn(k, "fw"))
    await asyncio.wait_for(model.entered.wait(), 5)
    forget = asyncio.ensure_future(_call(k, "fw", {"type": "context_forget"}))
    await asyncio.sleep(0.1)
    assert not forget.done(), "it waits for the turn"
    model.hold.set()
    await turn
    out = await asyncio.wait_for(forget, 5)
    assert out["pruned"] >= 0
