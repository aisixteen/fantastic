"""`fork`, `mark`, `fork_points` (#425): a new agent from the first N rows of one. The copy
keeps the ids, the card and grants match, the prefix assembles to the same prompt; every
refusal names its reason and leaves nothing behind. Port of Rust `fork.rs` and
`tests_fork.rs`: each test is `test_<rust_test>`."""

from __future__ import annotations

import json

from _testkit import as_agent, memory_meta, wire

from ai_core import constants, core, folds

DONE = '<call name="done"></call>'
REFLECT = '<call name="send"><target_id>kernel</target_id><type>reflect</type></call>'


class _Done:
    """Answers DONE to every pass."""

    def __init__(self):
        self.calls = 0

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        yield DONE


def _agent(kernel, aid, **extra):
    kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        model="test-model",
        endpoint="http://127.0.0.1:1",
        **memory_meta(kernel, aid, {"cli": "*"}),
        **extra,
    )
    return aid


async def _verb(kernel, aid, payload) -> dict:
    return (await kernel.send([{"target": aid, "payload": payload}]))[0]["result"]


async def _store(kernel, aid) -> list[dict]:
    with as_agent(kernel, aid):
        res = await core._store_call(aid, kernel, {"type": "read"})
    return res.get("rows") or []


def _ids(rows) -> list[int]:
    return [r["id"] for r in rows]


async def _put(kernel, aid, row) -> int:
    row = {**row, "id": kernel.ctx.mint_id()}
    with as_agent(kernel, aid):
        await core._save_history(aid, kernel, [row])
    return row["id"]


async def _turn(kernel, aid, q, a) -> tuple[int, int]:
    u = await _put(kernel, aid, {"role": "user", "content": q})
    r = await _put(kernel, aid, {"role": "assistant", "content": a, "parent": u})
    return u, r


async def _three_turns(kernel, aid) -> list[int]:
    out: list[int] = []
    for q, a in (
        ("question one about the parser", "answer one, tokens"),
        ("question two about the lexer", "answer two, lines"),
        ("question three about the printer", "answer three, bytes"),
    ):
        out.extend(await _turn(kernel, aid, q, a))
    return out


async def _fold_over(kernel, aid, members) -> int:
    fid = kernel.ctx.mint_id()
    row = folds.make_fold(
        aid, members, "the span", "summary", False, id=fid, anchor=members[0]
    )
    with as_agent(kernel, aid):
        await core._save_history(aid, kernel, [row])
    return fid


async def _prompt(kernel, aid) -> list[dict]:
    """What a provider would receive for the agent's next turn."""
    with as_agent(kernel, aid):
        msgs = (await core._assemble(aid, "next", kernel, "c", system_prompt="sys"))[0]
    return core._render_for_model(msgs)


def _exists(kernel, aid) -> bool:
    return aid in kernel.ctx.agents


async def test_fork_copies_rows_up_to_at_with_their_ids(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_copy")
    allr = await _three_turns(k, parent)
    at = allr[3]
    out = await _verb(k, parent, {"type": "fork", "at": at})
    assert out.get("copied") == 4, out
    assert out["at"] == at, out
    theirs = await _store(k, out["id"])
    assert _ids(theirs) == allr[:4], "ids kept, in order, none above"
    assert theirs == (await _store(k, parent))[:4], "the rows are the parent's"


async def test_fork_copies_a_whole_fold_with_its_members(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_fold")
    allr = await _three_turns(k, parent)
    # the first turn is folded AFTER the second one ran: the fold's id is above `at`
    fid = await _fold_over(k, parent, allr[:2])
    out = await _verb(k, parent, {"type": "fork", "at": allr[1]})
    assert out.get("copied") == 3, out
    theirs = await _store(k, out["id"])
    assert _ids(theirs) == [allr[0], allr[1], fid], "members, then the fold"
    assert theirs[2]["refs"] == [allr[0], allr[1]]


async def test_fork_refuses_an_at_that_cuts_a_fold(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_cutfold")
    allr = await _three_turns(k, parent)
    await _fold_over(k, parent, allr[:4])
    # `at` ends turn one: two of the fold's four members are before it, two after
    out = await _verb(k, parent, {"type": "fork", "at": allr[1], "id": "fk_cutfold_new"})
    assert out["reason"] == "cuts_fold", out
    assert out["at"] == allr[1], out
    assert not _exists(k, "fk_cutfold_new"), "no agent is created"


async def test_fork_refuses_an_at_that_cuts_a_turn(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_cutturn")
    u = await _put(k, parent, {"role": "user", "content": "go"})
    a = await _put(
        k, parent, {"role": "assistant", "content": REFLECT, "parent": u}
    )
    r = await _put(k, parent, {"role": "user", "content": "[reply]", "parent": a})
    done = await _put(
        k, parent, {"role": "assistant", "content": "fine", "parent": r}
    )
    # between the call and its reply, and between the question and its answer
    for at in (a, u, r):
        out = await _verb(k, parent, {"type": "fork", "at": at, "id": "fk_cutturn_new"})
        assert out["reason"] == "cuts_turn", (at, out)
        assert not _exists(k, "fk_cutturn_new")
    # the end of the finished turn is a boundary
    out = await _verb(k, parent, {"type": "fork", "at": done})
    assert out.get("copied") == 4, out


async def test_fork_refuses_an_open_chain(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_open")
    u = await _put(k, parent, {"role": "user", "content": "go"})
    a = await _put(k, parent, {"role": "assistant", "content": REFLECT, "parent": u})
    out = await _verb(k, parent, {"type": "fork", "at": a})
    assert out["reason"] == "cuts_turn", out


async def test_fork_refuses_an_at_that_names_no_row(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_norow")
    await _three_turns(k, parent)
    out = await _verb(k, parent, {"type": "fork", "at": 999_999})
    assert out["reason"] == "no_such_row", out
    assert out["at"] == 999_999, out
    # a view row is no conversation row either
    view = await _put(k, parent, {"kind": "view", "ids": []})
    out = await _verb(k, parent, {"type": "fork", "at": view})
    assert out["reason"] == "no_such_row", out
    for bad in (None, "x", -1):
        out = await _verb(k, parent, {"type": "fork", "at": bad})
        assert out["reason"] == "bad_arg", out
        assert out["arg"] == "at", out


async def test_fork_refuses_while_the_parent_is_mid_turn(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_busy")
    allr = await _three_turns(k, parent)
    async with core._lock_for(parent):
        out = await _verb(
            k, parent, {"type": "fork", "at": allr[1], "id": "fk_busy_new"}
        )
    assert out["reason"] == "parent_mid_turn", out
    assert not _exists(k, "fk_busy_new")


async def test_the_parents_later_rows_folds_and_forgets_do_not_reach_the_fork(
    seeded_kernel,
):
    k = seeded_kernel
    parent = _agent(k, "fk_later")
    allr = await _three_turns(k, parent)
    out = await _verb(k, parent, {"type": "fork", "at": allr[3]})
    fork = out["id"]
    before = await _store(k, fork)
    # the parent goes on: a new turn, a fold of the whole history, a forget
    await _turn(k, parent, "question four", "answer four")
    folded = await _verb(
        k, parent, {"type": "context_fold", "summary": "everything so far"}
    )
    assert isinstance(folded.get("folded"), int), folded
    forgot = await _verb(k, parent, {"type": "context_forget"})
    assert forgot["pruned"] > 0, forgot
    assert await _store(k, fork) == before, "the fork is as it was"


async def test_the_forks_rows_do_not_reach_the_parent(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_back")
    allr = await _three_turns(k, parent)
    out = await _verb(k, parent, {"type": "fork", "at": allr[3]})
    fork = out["id"]
    before = await _store(k, parent)
    await _turn(k, fork, "only the fork asks", "only the fork answers")
    assert await _store(k, parent) == before, "the parent is as it was"
    assert len(await _store(k, fork)) == 6


async def test_the_fork_has_the_parents_card_and_grants(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_card")
    k.ctx.update(
        parent,
        system_prompt="you are the parser agent",
        endpoint="http://127.0.0.1:2",
        output_reserve=512,
    )
    allr = await _three_turns(k, parent)
    out = await _verb(k, parent, {"type": "fork", "at": allr[1]})
    fork = out["id"]
    assert fork != parent
    p, f = k.get(parent), k.get(fork)
    for key in (
        "model",
        "endpoint",
        "system_prompt",
        "memory_store_id",
        "context_window",
        "output_reserve",
        "can_call",
        "handler_module",
    ):
        assert f.get(key) == p.get(key), key
        assert f.get(key) is not None, f"{key} is on the card"
    assert fork in k.get("memory")["callable_by"], "the store admits the fork"
    # a name and an id are taken as given
    named = await _verb(
        k,
        parent,
        {"type": "fork", "at": allr[1], "name": "the twin", "id": "fk_card_twin"},
    )
    assert named["id"] == "fk_card_twin", named
    assert k.get("fk_card_twin")["display_name"] == "the twin"


async def test_fork_is_all_or_nothing_on_a_store_refusal(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_refused")
    allr = await _three_turns(k, parent)
    # the store lets the parent read and prune, not append: the fork inherits that
    k.ctx.edit_callable_by(
        "memory", lambda cb: cb.__setitem__(parent, ["read", "prune"]) or True
    )
    out = await _verb(
        k, parent, {"type": "fork", "at": allr[3], "id": "fk_refused_new"}
    )
    assert out["reason"] == "store_refused", out
    assert not _exists(k, "fk_refused_new"), "the agent is deleted"
    dump = await _verb(k, "memory", {"type": "dump"})
    assert all(
        not r["owner"].endswith("fk_refused_new") for r in dump["tables"]["rows"]
    ), "the store holds none of its rows"
    assert "fk_refused_new" not in k.get("memory")["callable_by"], "grant taken back"


async def test_the_forks_prompt_equals_the_parents_for_the_same_prefix(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fk_prompt")
    reference = _agent(k, "fk_prompt_ref")
    # turn one, a fold of it, turn two with the view row written before its answer
    # (the fold and the view are both at or before `at`), then a third turn
    u1, a1 = await _turn(k, parent, "question one about the parser", "answer one")
    fid = await _fold_over(k, parent, [u1, a1])
    u2 = await _put(k, parent, {"role": "user", "content": "question two"})
    view = await _put(
        k,
        parent,
        {
            "kind": "view",
            "v": 1,
            "budget": 1000,
            "why": "t",
            "ids": [{"id": fid, "res": "fold"}, {"id": u2, "res": "full"}],
        },
    )
    at = await _put(
        k, parent, {"role": "assistant", "content": "answer two", "parent": u2}
    )
    await _turn(k, parent, "question three", "answer three")

    out = await _verb(k, parent, {"type": "fork", "at": at})
    fork = out["id"]
    assert _ids(await _store(k, fork)) == [u1, a1, fid, u2, view, at]
    # plant the reference with exactly the parent's rows up to `at` (same ids), by hand
    mine = [r for r in await _store(k, parent) if r["id"] <= at]
    with as_agent(k, reference):
        await core._save_history(reference, k, mine)
    theirs, wanted = await _prompt(k, fork), await _prompt(k, reference)
    assert len(theirs) > 3, theirs
    assert json.dumps(theirs) == json.dumps(wanted), "byte for byte"
    assert "[fold " in json.dumps(theirs), "the fold is in the prompt"


async def test_mark_lands_on_a_turn_boundary_outside_a_fold(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "mk_edge")
    await _three_turns(k, parent)
    # mid-turn: refused
    async with core._lock_for(parent):
        out = await _verb(k, parent, {"type": "mark", "label": "busy"})
    assert out["reason"] == "mid_turn", out
    # a label is required
    for bad in (None, "", "  ", 3):
        out = await _verb(k, parent, {"type": "mark", "label": bad})
        assert out["reason"] == "bad_arg", out
        assert out["arg"] == "label", out
    # inside an open chain: refused
    u = await _put(k, parent, {"role": "user", "content": "go"})
    await _put(k, parent, {"role": "assistant", "content": REFLECT, "parent": u})
    out = await _verb(k, parent, {"type": "mark", "label": "open"})
    assert out["reason"] == "open_chain", out

    # a head that ends in a fold is a valid place, and `fork` accepts its `at`
    parent2 = _agent(k, "mk_edge_fold")
    all2 = await _three_turns(k, parent2)
    fid = await _fold_over(k, parent2, all2)
    out = await _verb(
        k,
        parent2,
        {"type": "mark", "label": "after the fold", "note": "n", "version": "abc123"},
    )
    assert out["at"] == fid, out
    assert out["label"] == "after the fold", out
    assert out["marked"] > fid, out
    forked = await _verb(k, parent2, {"type": "fork", "at": out["at"]})
    assert forked.get("copied") == 7, forked


async def test_mark_keeps_only_the_last_n_marks(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "mk_last")
    await _three_turns(k, parent)
    n = constants.MAX_MARKS
    for i in range(n + 2):
        out = await _verb(k, parent, {"type": "mark", "label": f"m{i}"})
        assert isinstance(out["marked"], int), out
    marks = [r for r in await _store(k, parent) if r.get("kind") == "mark"]
    assert len(marks) == n
    assert marks[0]["label"] == "m2", "the two oldest are gone"
    assert marks[-1]["label"] == f"m{n + 1}"


async def test_mark_rows_never_reach_the_prompt(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "mk_hidden")
    await _three_turns(k, parent)
    before = json.dumps(await _prompt(k, parent))
    out = await _verb(
        k, parent, {"type": "mark", "label": "SECRETLABEL", "note": "SECRETNOTE"}
    )
    assert isinstance(out["marked"], int), out
    after = json.dumps(await _prompt(k, parent))
    assert "SECRETLABEL" not in after and "SECRETNOTE" not in after
    assert before == after, "the prompt is the same with the mark"


async def test_fork_points_lists_this_agents_marks_with_rows_after_model_and_warm(
    seeded_kernel,
):
    import time

    k = seeded_kernel
    parent = _agent(k, "fp_list")
    _, a1 = await _turn(k, parent, "one", "ok one")
    first = await _verb(
        k,
        parent,
        {
            "type": "mark",
            "label": "context read",
            "note": "before edits",
            "version": "c0ffee",
        },
    )
    assert first["at"] == a1, first
    await _turn(k, parent, "two", "ok two")
    _, a3 = await _turn(k, parent, "three", "ok three")
    await _verb(
        k, parent, {"type": "mark", "label": "issue 7 merged", "version": "deadbe"}
    )

    out = await _verb(k, parent, {"type": "fork_points"})
    points = out["points"]
    assert len(points) == 2, out
    assert points[0] == {
        "agent": parent,
        "at": a1,
        "label": "context read",
        "note": "before edits",
        "version": "c0ffee",
        "rows_after": 4,
        "model": "test-model",
        "warm": False,
    }
    assert points[1]["at"] == a3
    assert points[1]["rows_after"] == 0
    assert points[1]["note"] is None

    # WARM while the last send ended inside the cache lifetime, cold after
    now = time.time()
    core._last_send[parent] = now - 10.0
    out = await _verb(k, parent, {"type": "fork_points"})
    assert out["points"][0]["warm"] is True, out
    core._last_send[parent] = now - constants.CACHE_LIFETIME_SECS - 1.0
    out = await _verb(k, parent, {"type": "fork_points"})
    assert out["points"][0]["warm"] is False, out

    # the filter: a substring of the label, the exact version
    by_label = await _verb(
        k, parent, {"type": "fork_points", "filter": {"label": "merged"}}
    )
    assert [p["label"] for p in by_label["points"]] == ["issue 7 merged"]
    by_version = await _verb(
        k, parent, {"type": "fork_points", "filter": {"version": "c0ffee"}}
    )
    assert by_version["points"][0]["label"] == "context read"
    none = await _verb(
        k, parent, {"type": "fork_points", "filter": {"version": "c0ff"}}
    )
    assert none["points"] == [], "a version is matched whole"


async def test_a_finished_send_makes_the_agent_warm(seeded_kernel, fake_providers):
    k = seeded_kernel
    parent = _agent(k, "fp_warm")
    await _turn(k, parent, "one", "ok one")
    await _verb(k, parent, {"type": "mark", "label": "m"})
    cold = await _verb(k, parent, {"type": "fork_points"})
    assert cold["points"][0]["warm"] is False, cold
    fake_providers[parent] = _Done()
    await _verb(k, parent, {"type": "send", "text": "go"})
    assert fake_providers[parent].calls >= 1
    warm = await _verb(k, parent, {"type": "fork_points"})
    assert warm["points"][0]["warm"] is True, warm


async def test_two_forks_of_one_mark_start_from_the_same_rows(seeded_kernel):
    k = seeded_kernel
    parent = _agent(k, "fp_twins")
    await _three_turns(k, parent)
    mark = await _verb(k, parent, {"type": "mark", "label": "base"})
    await _turn(k, parent, "later", "ok later")
    point = (await _verb(k, parent, {"type": "fork_points"}))["points"][0]
    assert point["at"] == mark["at"]
    a = await _verb(k, parent, {"type": "fork", "at": point["at"]})
    b = await _verb(k, parent, {"type": "fork", "at": point["at"]})
    assert a["id"] != b["id"]
    rows = await _store(k, a["id"])
    assert len(rows) == 6
    assert rows == await _store(k, b["id"]), "ids and content alike"


async def test_fork_from_forks_the_named_agent(seeded_kernel):
    # `fork {from:{agent, at}}` on a peer: the same verb, run on the agent named
    k = seeded_kernel
    parent = _agent(k, "fk_from")
    peer = _agent(k, "fk_from_peer")
    wire(k, peer, parent)
    allr = await _three_turns(k, parent)
    out = await _verb(
        k, peer, {"type": "fork", "from": {"agent": parent, "at": allr[1]}}
    )
    assert out.get("copied") == 2, out
    assert _ids(await _store(k, out["id"])) == allr[:2]
    bad = await _verb(k, peer, {"type": "fork", "from": {"agent": 3}})
    assert bad["reason"] == "bad_arg", bad


async def test_fork_and_mark_refusals_put_sentence_in_error_code_in_reason(
    seeded_kernel,
):
    k = seeded_kernel
    parent = _agent(k, "fk_shape")
    u = await _put(k, parent, {"role": "user", "content": "go"})
    a = await _put(k, parent, {"role": "assistant", "content": REFLECT, "parent": u})
    r = await _put(k, parent, {"role": "user", "content": "[reply]", "parent": a})
    await _put(k, parent, {"role": "assistant", "content": "fine", "parent": r})
    out = await _verb(k, parent, {"type": "fork", "at": a})
    assert out["reason"] == "cuts_turn", out
    assert out["error"] != out["reason"] and " " in out["error"], out
    assert isinstance(out["hint"], str) and "at" in out, out
    async with core._lock_for(parent):
        out = await _verb(k, parent, {"type": "mark", "label": "busy"})
    assert out["reason"] == "mid_turn", out
    assert out["error"] != out["reason"], out
