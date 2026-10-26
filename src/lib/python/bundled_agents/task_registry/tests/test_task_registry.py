"""Unit tests — the Python twin of the rust crate's `tests.rs`; every test is named
`test_<rust test>`. Driven through `kernel.send`, as production is. A `tr_caller.tools`
agent stands for any agent that makes calls (a lead, a worker); the store is a real
`ram_store`."""

from __future__ import annotations

import types
from pathlib import Path

import pytest

from _testkit import ensure_memory
from kernel import register_unlisted
from kernel._bundles import bundle_enabled
from task_registry import tools

#: Every verb a spy agent received: (agent, verb).
SPIED: list = []


async def _caller_handler(id, payload, kernel):
    """Make the call it is told to (`call {target, payload}`) AS ITSELF, so the gate
    and the registry see a real agent. Every other verb answers ok."""
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        return {"id": id, "sentence": "a caller"}
    if verb == "call":
        out = await kernel.send(
            [{"target": payload["target"], "payload": payload["payload"]}]
        )
        return out[0]["result"]
    return {"ok": True, "verb": verb}


async def _spy_handler(id, payload, kernel):
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    SPIED.append((id, verb))
    return {"ok": True}


register_unlisted("tr_caller.tools", types.SimpleNamespace(handler=_caller_handler))
register_unlisted("tr_spy.tools", types.SimpleNamespace(handler=_spy_handler))

WRITE_VERBS = ["add", "assign", "reassign", "status", "report", "list", "get"]


@pytest.fixture(autouse=True)
def _on(monkeypatch):
    monkeypatch.setenv("FANTASTIC_TASK_REGISTRY", "1")


async def send(kernel, target, payload):
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def create(kernel, **payload):
    payload.setdefault("parent_id", "kernel_state")
    r = await send(kernel, "kernel_state", {"type": "create_agent", **payload})
    assert "error" not in r, r
    return r


class Rig:
    def __init__(self, kernel, store):
        self.kernel, self.reg, self.store = kernel, "tr", store

    async def make_registry(self, rid):
        """A registry agent `rid` on the rig's store, wired both ends."""
        ctx = self.kernel.ctx
        await create(
            self.kernel,
            handler_module="task_registry.tools",
            id=rid,
            task_store_id=self.store,
            can_call={self.store: "*"},
            callable_by={"lead": "*", "sublead": "*"},
        )
        cb = dict(ctx.agents[self.store]._meta.get("callable_by") or {})
        cb[rid] = "*"
        ctx.update(self.store, callable_by=cb)
        for who in ("lead", "sublead"):
            cc = dict(ctx.agents[who]._meta.get("can_call") or {})
            cc[rid] = list(WRITE_VERBS)
            ctx.update(who, can_call=cc)

    async def op(self, payload):
        return await send(self.kernel, self.reg, payload)

    async def call(self, who, payload):
        return await self.call_on(who, self.reg, payload)

    async def call_on(self, who, target, payload):
        return await send(
            self.kernel, who, {"type": "call", "target": target, "payload": payload}
        )

    async def dump_rows(self):
        d = await send(self.kernel, self.store, {"type": "dump"})
        return d["tables"]["rows"]

    async def add(self, who, tid, **extra):
        return await self.call(
            who, {"type": "add", "id": tid, "source": f"src:{tid}", **extra}
        )


async def rig(kernel):
    """`lead` -> `sublead` -> `worker`, a `ram_store`, and a registry on it. The leads
    may call the registry; the worker holds no grant on it."""
    store = ensure_memory(kernel)
    grant = {"tr": list(WRITE_VERBS)}
    await create(kernel, handler_module="tr_caller.tools", id="lead", can_call=grant)
    await create(
        kernel,
        handler_module="tr_caller.tools",
        id="sublead",
        parent_id="lead",
        can_call=grant,
    )
    await create(
        kernel, handler_module="tr_caller.tools", id="worker", parent_id="sublead"
    )
    r = Rig(kernel, store)
    await r.make_registry("tr")
    return r


def _ids(v):
    return [t["id"] for t in v["tasks"]]


def test_disabled_by_default(monkeypatch):
    monkeypatch.delenv("FANTASTIC_TASK_REGISTRY", raising=False)
    assert not bundle_enabled("task_registry.tools")
    monkeypatch.setenv("FANTASTIC_TASK_REGISTRY", "0")
    assert not bundle_enabled("task_registry.tools")
    monkeypatch.setenv("FANTASTIC_TASK_REGISTRY", "")
    assert not bundle_enabled("task_registry.tools")
    monkeypatch.setenv("FANTASTIC_TASK_REGISTRY", "1")
    assert bundle_enabled("task_registry.tools")
    monkeypatch.setenv("FANTASTIC_TASK_REGISTRY", "TRUE")
    assert bundle_enabled("task_registry.tools")


async def test_refuses_every_verb_without_task_store_id(kernel):
    await rig(kernel)
    await create(kernel, handler_module="task_registry.tools", id="bare")
    refl = await send(kernel, "bare", {"type": "reflect"})
    assert refl["enabled"] is False, refl
    for verb in WRITE_VERBS:
        out = await send(kernel, "bare", {"type": verb, "id": "t"})
        assert out["error"] == "disabled", (verb, out)


async def test_add_appends_one_row_to_the_store_agent(kernel):
    r = await rig(kernel)
    added = await r.add("lead", "t1", source="gh:o/r#12", holder="sublead")
    assert added["id"] == "t1", added
    rows = [x for x in await r.dump_rows() if x["row"]["kind"] == "task"]
    assert len(rows) == 1, rows
    assert rows[0]["owner"].endswith("|tr"), rows[0]["owner"]
    assert rows[0]["row"]["task_id"] == "t1"
    assert rows[0]["row"]["content"] == added
    assert added["source"] == "gh:o/r#12"
    assert added["status"] == "waiting"
    assert added["holder"] == "sublead"
    got = await r.call("lead", {"type": "get", "id": "t1"})
    assert got == {**added, "children": []}


async def test_add_refuses_duplicate_id_and_unknown_parent(kernel):
    r = await rig(kernel)
    assert (await r.add("lead", "t1"))["id"] == "t1"
    dup = await r.add("lead", "t1")
    assert dup["reason"] == "exists", dup
    orphan = await r.add("lead", "t2", parent="nope")
    assert orphan["reason"] == "not_found", orphan
    assert len((await r.call("lead", {"type": "list"}))["tasks"]) == 1


async def test_get_unknown_is_not_found(kernel):
    r = await rig(kernel)
    out = await r.call("lead", {"type": "get", "id": "ghost"})
    assert out["reason"] == "not_found", out


async def test_source_is_opaque_and_nothing_is_pushed(kernel):
    r = await rig(kernel)
    SPIED.clear()
    await create(
        kernel, handler_module="tr_spy.tools", id="spy", callable_by={"tr": "*"}
    )
    kernel.ctx.update(
        "tr", can_call={**kernel.ctx.agents["tr"]._meta["can_call"], "spy": "*"}
    )
    for i, src in enumerate(["gh:o/r#12", "jira:KEY-5", "9f3 !? random \U0001f600"]):
        out = await r.add("lead", f"t{i}", source=src)
        assert out["source"] == src, out
        assert out["update_to"] == src, "update_to defaults to source"
    own = await r.add("lead", "t9", source="gh:o/r#1", update_to="jira:KEY-9")
    assert own["update_to"] == "jira:KEY-9"
    await r.call("lead", {"type": "status", "id": "t0", "status": "done"})
    await r.call("lead", {"type": "assign", "id": "t0", "holder": "worker"})
    assert not [s for s in SPIED if s[0] == "spy"], "the registry called a source agent"
    rows = await r.dump_rows()
    assert len(rows) == 6, f"4 adds + status + assign: {rows}"


async def test_assign_sets_holder_and_history(kernel):
    r = await rig(kernel)
    await r.add("lead", "t1")
    a = await r.call("lead", {"type": "assign", "id": "t1", "holder": "sublead"})
    assert a["holder"] == "sublead", a
    h = a["history"]
    assert len(h) == 1
    assert (h[0]["from"], h[0]["to"], h[0]["by"]) == (None, "sublead", "lead")
    assert h[0]["at"] > 0
    again = await r.call("lead", {"type": "assign", "id": "t1", "holder": "worker"})
    assert again["error"] == "already_held", again
    got = await r.call("lead", {"type": "get", "id": "t1"})
    assert got["holder"] == "sublead"
    assert len(got["history"]) == 1
    await r.add("lead", "t2")
    op = await r.op({"type": "assign", "id": "t2", "holder": "worker"})
    assert op["history"][0]["by"] is None, op


async def test_reassign_requires_a_holder(kernel):
    r = await rig(kernel)
    await r.add("lead", "t1")
    out = await r.call("lead", {"type": "reassign", "id": "t1", "holder": "worker"})
    assert out["error"] == "not_held", out
    got = await r.call("lead", {"type": "get", "id": "t1"})
    assert got["holder"] is None
    assert got["history"] == []


async def test_reassign_agent_to_agent_and_to_subtree_and_back_keeps_history(kernel):
    r = await rig(kernel)
    await r.add("lead", "t1")
    await r.call("lead", {"type": "assign", "id": "t1", "holder": "sublead"})
    two = await r.call(
        "sublead",
        {"type": "reassign", "id": "t1", "holder": {"subtree": "lead"}},
    )
    assert two["holder"] == {"subtree": "lead"}, two
    three = await r.call("lead", {"type": "reassign", "id": "t1", "holder": "worker"})
    assert three["holder"] == "worker", three
    steps = [(x["from"], x["to"], x["by"]) for x in three["history"]]
    assert steps == [
        (None, "sublead", "lead"),
        ("sublead", {"subtree": "lead"}, "sublead"),
        ({"subtree": "lead"}, "worker", "lead"),
    ]


async def test_holder_must_exist(kernel):
    r = await rig(kernel)
    add = await r.add("lead", "t0", holder="nobody")
    assert add["reason"] == "not_found", add
    await r.add("lead", "t1")
    a = await r.call("lead", {"type": "assign", "id": "t1", "holder": "nobody"})
    assert a["reason"] == "not_found", a
    sub = await r.call(
        "lead", {"type": "assign", "id": "t1", "holder": {"subtree": "nobody"}}
    )
    assert sub["reason"] == "not_found", sub
    await r.call("lead", {"type": "assign", "id": "t1", "holder": "worker"})
    re = await r.call("lead", {"type": "reassign", "id": "t1", "holder": "nobody"})
    assert re["reason"] == "not_found", re
    got = await r.call("lead", {"type": "get", "id": "t1"})
    assert got["holder"] == "worker", "unchanged"


async def test_status_sets_and_rejects_unknown(kernel):
    r = await rig(kernel)
    await r.add("lead", "t1")
    for s in tools.STATES:
        out = await r.call("lead", {"type": "status", "id": "t1", "status": s})
        assert out["status"] == s, out
    bad = await r.call("lead", {"type": "status", "id": "t1", "status": "finished"})
    assert bad["reason"] == "bad_arg", bad
    got = await r.call("lead", {"type": "get", "id": "t1"})
    assert got["status"] == "paused", "unchanged by the bad one"
    bad_add = await r.add("lead", "t2", status="nope")
    assert bad_add["reason"] == "bad_arg", bad_add
    ghost = await r.call("lead", {"type": "status", "id": "ghost", "status": "done"})
    assert ghost["reason"] == "not_found"


async def test_report_records_last_report_from_own_subtree(kernel):
    r = await rig(kernel)
    await r.add("lead", "t1", holder="worker")
    one = await r.call(
        "lead",
        {
            "type": "report",
            "id": "t1",
            "from": "worker",
            "state": "running",
            "result": {"pct": 40},
        },
    )
    rep = one["report"]
    assert (rep["from"], rep["state"], rep["result"]) == (
        "worker",
        "running",
        {"pct": 40},
    ), one
    assert rep["at"] > 0
    two = await r.call(
        "sublead", {"type": "report", "id": "t1", "state": "done", "result": "ok"}
    )
    assert two["report"]["from"] == "sublead", two
    got = await r.call("lead", {"type": "get", "id": "t1"})
    assert got["report"] == two["report"]
    assert got["report"]["state"] == "done"
    bad = await r.call("lead", {"type": "report", "id": "t1", "state": "finished"})
    assert bad["reason"] == "bad_arg", bad


async def test_report_for_an_agent_outside_the_callers_subtree_is_refused(kernel):
    r = await rig(kernel)
    await r.add("lead", "t1")
    up = await r.call(
        "sublead", {"type": "report", "id": "t1", "from": "lead", "state": "done"}
    )
    assert up["error"] == "not_in_subtree", up
    ghost = await r.call(
        "lead", {"type": "report", "id": "t1", "from": "nobody", "state": "done"}
    )
    assert ghost["reason"] == "not_found", ghost
    got = await r.call("lead", {"type": "get", "id": "t1"})
    assert got["report"] is None, "the entry is unchanged"
    assert len(await r.dump_rows()) == 1, "no row was written"


async def test_list_filters_by_parent_holder_and_status(kernel):
    r = await rig(kernel)
    await r.add("lead", "b", holder="sublead")
    await r.add("lead", "a", parent="b", holder={"subtree": "sublead"})
    await r.add("lead", "c", parent="b", status="running")
    assert _ids(await r.call("lead", {"type": "list"})) == ["a", "b", "c"]
    assert _ids(await r.call("lead", {"type": "list", "parent": "b"})) == ["a", "c"]
    assert _ids(await r.call("lead", {"type": "list", "holder": "sublead"})) == ["b"]
    assert _ids(
        await r.call("lead", {"type": "list", "holder": {"subtree": "sublead"}})
    ) == ["a"]
    assert _ids(await r.call("lead", {"type": "list", "status": "running"})) == ["c"]
    assert _ids(
        await r.call("lead", {"type": "list", "parent": "b", "status": "waiting"})
    ) == ["a"]


async def test_get_lists_children_and_history(kernel):
    r = await rig(kernel)
    await r.add("lead", "top")
    await r.add("lead", "mid", parent="top")
    await r.add("lead", "leaf", parent="mid")
    await r.add("lead", "mid2", parent="top")
    await r.call("lead", {"type": "assign", "id": "mid", "holder": "sublead"})
    await r.call("lead", {"type": "reassign", "id": "mid", "holder": "worker"})
    top = await r.call("lead", {"type": "get", "id": "top"})
    assert top["children"] == ["mid", "mid2"], top
    mid = await r.call("lead", {"type": "get", "id": "mid"})
    assert mid["children"] == ["leaf"]
    assert mid["parent"] == "top"
    assert len(mid["history"]) == 2
    leaf = await r.call("lead", {"type": "get", "id": "leaf"})
    assert leaf["children"] == []
    assert leaf["parent"] == "mid"


async def test_worker_without_a_grant_cannot_write(kernel):
    r = await rig(kernel)
    await r.add("lead", "t1")
    for p in [
        {"type": "add", "id": "w", "source": "x"},
        {"type": "assign", "id": "t1", "holder": "worker"},
        {"type": "reassign", "id": "t1", "holder": "worker"},
        {"type": "status", "id": "t1", "status": "done"},
        {"type": "report", "id": "t1", "state": "done"},
    ]:
        out = await r.call("worker", p)
        assert out["reason"] == "unauthorized", (p, out)
    got = await r.call("lead", {"type": "get", "id": "t1"})
    assert got["holder"] is None
    assert got["status"] == "waiting"
    assert len(await r.dump_rows()) == 1, "only the lead's add"
    ok = await r.call("lead", {"type": "status", "id": "t1", "status": "done"})
    assert "error" not in ok, ok


async def test_state_survives_a_registry_restart(kernel):
    r = await rig(kernel)
    await r.add("lead", "t1")
    await r.add("lead", "t2", parent="t1")
    await r.call("lead", {"type": "assign", "id": "t2", "holder": "worker"})
    before = await r.call("lead", {"type": "list"})
    gone = await send(kernel, "kernel_state", {"type": "delete_agent", "id": "tr"})
    assert gone["deleted"] is True, gone
    await r.make_registry("tr")
    after = await r.call("lead", {"type": "list"})
    assert after == before
    assert len(after["tasks"]) == 2


async def test_two_registries_on_one_store_do_not_mix(kernel):
    r = await rig(kernel)
    await r.make_registry("tr2")
    await r.add("lead", "mine")
    other = await r.call_on(
        "lead", "tr2", {"type": "add", "id": "theirs", "source": "s"}
    )
    assert other["id"] == "theirs", other
    one = await r.call("lead", {"type": "list"})
    two = await r.call_on("lead", "tr2", {"type": "list"})
    assert _ids(one) == ["mine"]
    assert _ids(two) == ["theirs"]
    again = await r.call_on("lead", "tr2", {"type": "add", "id": "mine", "source": "s"})
    assert again["id"] == "mine"
    assert len(await r.dump_rows()) == 3


async def test_store_refusal_passes_through(kernel):
    r = await rig(kernel)
    ctx = kernel.ctx
    await r.add("lead", "t1")
    cb = dict(ctx.agents[r.store]._meta["callable_by"])
    ctx.update(r.store, callable_by={k: v for k, v in cb.items() if k != "tr"})
    out = await r.add("lead", "t2")
    assert out["error"] == "store_refused", out
    assert out["store_error"], f"carries the store's own error: {out}"
    assert out["store_reason"] == "unauthorized", out
    read = await r.call("lead", {"type": "list"})
    assert read["error"] == "store_refused", read
    ctx.update(r.store, callable_by=cb)
    listed = await r.call("lead", {"type": "list"})
    assert len(listed["tasks"]) == 1, listed


def test_readme_is_rust_verbatim():
    here = Path(tools.__file__).resolve().parent
    rust = (
        Path(__file__).resolve().parents[5]
        / "lib/rust/crates/bundles/fantastic-task-registry"
    )
    assert (here / "readme.md").read_text() == (rust / "src/readme.md").read_text()
